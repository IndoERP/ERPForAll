// SPDX-License-Identifier: MIT OR Apache-2.0
//! Native seller selection. PostgreSQL owns terms; all derived values are call-local.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};

use crate::defaults::{company_currency_id, env_company_id, opt_id};
use crate::product::ProductProduct;
use crate::supplierinfo::{decimal, required_id};
use erp_addon_base::money::{MAX_CURRENCIES_PER_CALL, RateBook};
use erp_sdk::money::{ExactRatio, Increment, RateDateRule, RoundingMode};
use erp_sdk::prelude::*;
use erp_sdk::{FieldType, ValueMap};

/// Validated native seller request. Missing quantity skips thresholds; zero does not.
///
/// ```no_run
/// use erp_addon_product::{ProductProduct, ProductSellerMethods, SellerQuery};
/// use erp_sdk::prelude::*;
/// use erp_sdk::ValueMap;
/// async fn vendor_for(product: &RecordSet<ProductProduct>, tx: &mut Transaction) -> Result<ValueMap> {
///     let query = SellerQuery::default().with_quantity(Some(Decimal::from(12)));
///     product._select_seller(tx, query).await
/// }
/// ```
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct SellerQuery {
    partner: Option<i64>,
    quantity: Option<Decimal>,
    date: Option<Date>,
    unit: Option<i64>,
    parameter_partner: Option<i64>,
    force_uom: bool,
    order: String,
}

impl Default for SellerQuery {
    fn default() -> Self {
        Self {
            partner: None,
            quantity: Some(Decimal::ZERO),
            date: None,
            unit: None,
            parameter_partner: None,
            force_uom: false,
            order: "price_discounted".into(),
        }
    }
}

impl SellerQuery {
    /// Requested partner, with its immediate parent also eligible.
    pub fn partner(&self) -> Option<i64> {
        self.partner
    }
    /// Requested quantity; None disables the threshold.
    pub fn quantity(&self) -> Option<Decimal> {
        self.quantity
    }
    /// Explicit date, or None for contextual today.
    pub fn date(&self) -> Option<Date> {
        self.date
    }
    /// Unit in which the quantity is expressed.
    pub fn unit(&self) -> Option<i64> {
        self.unit
    }
    /// Exact preparation-stage partner restriction.
    pub fn parameter_partner(&self) -> Option<i64> {
        self.parameter_partner
    }
    /// Whether vendor units are restricted to the requested or main product unit.
    pub fn force_uom(&self) -> bool {
        self.force_uom
    }
    /// Validated field identifier whose type/access is checked at execution.
    pub fn order(&self) -> &str {
        &self.order
    }
    /// Set the quantity; None is used when looking for a vendor regardless of quantity.
    pub fn with_quantity(mut self, quantity: Option<Decimal>) -> Self {
        self.quantity = quantity;
        self
    }
    /// Use an explicit date instead of contextual today.
    pub fn with_date(mut self, date: Date) -> Self {
        self.date = Some(date);
        self
    }
    /// Filter to this partner or its immediate parent.
    /// # Errors
    /// Validation for a nonpositive record identity.
    pub fn with_partner(mut self, partner: i64) -> Result<Self> {
        positive(partner, "partner")?;
        self.partner = Some(partner);
        Ok(self)
    }
    /// Interpret the quantity in this unit.
    /// # Errors
    /// Validation for a nonpositive record identity.
    pub fn with_unit(mut self, unit: i64) -> Result<Self> {
        positive(unit, "unit")?;
        self.unit = Some(unit);
        Ok(self)
    }
    /// Exact partner filter applied during preparation, before the partner/parent filter.
    /// # Errors
    /// Validation for a nonpositive record identity.
    pub fn with_parameter_partner(mut self, partner: i64) -> Result<Self> {
        positive(partner, "parameter partner")?;
        self.parameter_partner = Some(partner);
        Ok(self)
    }
    /// Keep only requested-unit and product-main-unit agreements.
    pub fn with_force_uom(mut self, force: bool) -> Self {
        self.force_uom = force;
        self
    }
    /// Give a readable scalar supplier field precedence over converted discounted price.
    /// # Errors
    /// Validation for an invalid PostgreSQL field identifier. Field/type/access validation
    /// occurs against the actual composed model when the query is executed.
    pub fn with_order(mut self, field: &str) -> Result<Self> {
        // PostgreSQL identifier bound, not an operator-configurable policy limit.
        if field.is_empty()
            || field.len() > 63
            || !field
                .bytes()
                .enumerate()
                .all(|(i, b)| b == b'_' || b.is_ascii_lowercase() || (i > 0 && b.is_ascii_digit()))
        {
            return Err(Error::validation(
                "Seller ordering must name one scalar supplier field.",
            ));
        }
        self.order = field.into();
        Ok(self)
    }
}

/// Native result with the unrounded normalized price alongside the ordinary mapping.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct SellerInfo {
    values: ValueMap,
    discounted_price: ExactRatio,
}

impl SellerInfo {
    pub(crate) fn new(values: ValueMap, discounted_price: ExactRatio) -> Self {
        Self {
            values,
            discounted_price,
        }
    }
    /// The nine documented agreement values; discounted price is rounded at Product Price precision.
    pub fn values(&self) -> &ValueMap {
        &self.values
    }
    /// Exact discounted price per product main unit, in the vendor's currency.
    pub fn discounted_price(&self) -> &ExactRatio {
        &self.discounted_price
    }
    /// Consume the result as its ordinary mapping.
    pub fn into_values(self) -> ValueMap {
        self.values
    }
}

fn positive(id: i64, name: &str) -> Result<()> {
    if RecId::from_real(id).is_none() {
        return Err(Error::validation(format!(
            "Seller {name} identity must be positive."
        )));
    }
    Ok(())
}

fn real(id: i64) -> Result<RecId> {
    RecId::from_real(id)
        .ok_or_else(|| Error::validation("Seller currency/company identity must be positive."))
}

fn integer(cell: Cell, field: &str) -> Result<i32> {
    match cell {
        Cell::Int(value) => Ok(value),
        other => Err(Error::programming(format!(
            "Supplier {field} returned {other:?}, expected an integer."
        ))),
    }
}

#[methods(ProductProduct, addon = "product")]
impl ProductProduct {
    /// Stable vendor priority, restricted by the supplier eligibility extension chain.
    #[method]
    async fn _prepare_sellers(
        this: &RecordSet<ProductProduct>,
        tx: &mut Transaction,
        query: SellerQuery,
    ) -> Result<Records> {
        this.ensure_one()?;
        let company = env_company_id(this.env(), "seller eligibility")?;
        let template = required_id(this.cell(tx, "product_tmpl_id").await?, "product_tmpl_id")?;
        let model = this.env().records("product.supplierinfo")?;
        let candidates = model
            .search(
                tx,
                &Domain::condition("product_tmpl_id", Operator::Eq, template)?,
            )
            .await?;
        let candidates: Records = candidates
            .call(
                tx,
                "_get_filtered_supplier",
                (company, this.ids()[0], query.parameter_partner),
            )
            .await?;
        let mut ordered = Vec::with_capacity(candidates.len());
        for id in candidates.rec_ids() {
            let row = candidates.browse_ids([*id]);
            ordered.push((
                (
                    integer(row.cell(tx, "sequence").await?, "sequence")?,
                    Reverse(decimal(row.cell(tx, "min_qty").await?, "min_qty")?),
                    decimal(row.cell(tx, "price").await?, "price")?,
                    id.get(),
                ),
                *id,
            ));
        }
        ordered.sort_by_key(|row| row.0);
        Ok(candidates.browse_ids(ordered.into_iter().map(|(_, id)| id)))
    }

    /// Date, partner, variant and quantity eligibility in the vendor unit.
    #[method]
    async fn _get_filtered_sellers(
        this: &RecordSet<ProductProduct>,
        tx: &mut Transaction,
        query: SellerQuery,
    ) -> Result<Records> {
        this.ensure_one()?;
        let date = match query.date {
            Some(date) => date,
            None => this.env().context_today()?,
        };
        let main_unit = required_id(this.cell(tx, "uom_id").await?, "uom_id")?;
        let parent = match query.partner {
            Some(partner) => opt_id(
                this.env()
                    .records("res.partner")?
                    .browse([partner])?
                    .cell(tx, "parent_id")
                    .await?,
                "parent_id",
            )?,
            None => None,
        };
        let digits =
            erp_addon_base::decimal_precision::precision_get(this.env(), tx, "Product Unit")
                .await?;
        let precision = Increment::from_places(digits)?;
        let candidates: Records = this
            .as_records()
            .call(tx, "_prepare_sellers", (query.clone(),))
            .await?;
        let mut ids = Vec::new();
        for id in candidates.rec_ids() {
            let row = candidates.browse_ids([*id]);
            let start = optional_date(row.cell(tx, "date_start").await?)?;
            let end = optional_date(row.cell(tx, "date_end").await?)?;
            if start.is_some_and(|start| start > date) || end.is_some_and(|end| end < date) {
                continue;
            }
            let unit = required_id(row.cell(tx, "uom_id").await?, "uom_id")?;
            if query.force_uom && query.unit != Some(unit) && unit != main_unit {
                continue;
            }
            let vendor = required_id(row.cell(tx, "partner_id").await?, "partner_id")?;
            if query
                .partner
                .is_some_and(|partner| partner != vendor && parent != Some(vendor))
            {
                continue;
            }
            if opt_id(row.cell(tx, "product_id").await?, "product_id")?
                .is_some_and(|variant| variant != this.ids()[0])
            {
                continue;
            }
            if let Some(quantity) = query.quantity {
                let quantity = match query.unit {
                    Some(source) if source != unit && !quantity.is_zero() => {
                        let source =
                            erp_addon_uom::models::load_unit(this.env(), tx, source).await?;
                        let target = erp_addon_uom::models::load_unit(this.env(), tx, unit).await?;
                        erp_addon_uom::conversion::quantity_rounded(
                            &source,
                            &target,
                            quantity,
                            &precision,
                            RoundingMode::AwayFromZero,
                        )?
                    }
                    // Reference leaves quantity unchanged when no source unit or zero is supplied.
                    _ => quantity,
                };
                if erp_addon_uom::conversion::precision_compare(
                    quantity,
                    decimal(row.cell(tx, "min_qty").await?, "min_qty")?,
                    &precision,
                )?
                .is_lt()
                {
                    continue;
                }
            }
            ids.push(*id);
        }
        Ok(candidates.browse_ids(ids))
    }

    /// First eligible vendor, then unrounded company-currency discounted price and tie breakers.
    #[method]
    async fn _select_seller(
        this: &RecordSet<ProductProduct>,
        tx: &mut Transaction,
        query: SellerQuery,
    ) -> Result<ValueMap> {
        let result: Option<SellerInfo> = this
            .as_records()
            .call(tx, "_select_seller_exact", (query,))
            .await?;
        Ok(match result {
            Some(result) => result.into_values(),
            None => ValueMap::new(),
        })
    }

    /// Selection with an exact normalized vendor price retained for native consumers.
    #[method]
    async fn _select_seller_exact(
        this: &RecordSet<ProductProduct>,
        tx: &mut Transaction,
        mut query: SellerQuery,
    ) -> Result<Option<SellerInfo>> {
        this.ensure_one()?;
        let date = match query.date {
            Some(date) => date,
            None => this.env().context_today()?,
        };
        query.date = Some(date);
        let model = this.env().records("product.supplierinfo")?;
        validate_order(&model, &query.order)?;
        let candidates: Records = this
            .as_records()
            .call(tx, "_get_filtered_sellers", (query.clone(),))
            .await?;
        let Some(first) = candidates.rec_ids().first() else {
            return Ok(None);
        };
        let vendor = required_id(
            candidates
                .browse_ids([*first])
                .cell(tx, "partner_id")
                .await?,
            "partner_id",
        )?;
        let mut rows = Vec::new();
        let mut currencies = BTreeSet::new();
        for id in candidates.rec_ids() {
            let row = candidates.browse_ids([*id]);
            if required_id(row.cell(tx, "partner_id").await?, "partner_id")? != vendor {
                continue;
            }
            let currency = required_id(row.cell(tx, "currency_id").await?, "currency_id")?;
            let price: ExactRatio = row.call(tx, "_get_discounted_price", ()).await?;
            let order = if query.order == "price_discounted" {
                SortValue::Null
            } else {
                order_value(row.cell(tx, &query.order).await?, &query.order)?
            };
            let sequence = integer(row.cell(tx, "sequence").await?, "sequence")?;
            currencies.insert(currency);
            rows.push((row, currency, price, order, sequence, id.get()));
        }
        let company = env_company_id(this.env(), "seller price conversion")?;
        let target = company_currency_id(tx, this.env(), company).await?;
        let factors = currency_factors(this.env(), tx, company, target, date, currencies).await?;
        let mut ordered = Vec::with_capacity(rows.len());
        for (row, currency, price, order, sequence, id) in rows {
            let factor = factors
                .get(&currency)
                .ok_or_else(|| Error::programming("Seller currency factor was not loaded."))?;
            let converted = price.checked_mul(factor).map_err(|error| {
                Error::validation(format!("Vendor agreement {id}: {error}")).with_source(error)
            })?;
            ordered.push(((order, converted, sequence, id), row));
        }
        ordered.sort_by(|a, b| a.0.cmp(&b.0));
        match ordered.into_iter().next() {
            Some((_, row)) => row.call(tx, "_get_seller_info_exact", ()).await,
            None => Ok(None),
        }
    }
}

fn optional_date(cell: Cell) -> Result<Option<Date>> {
    match cell {
        Cell::Date(date) => Ok(Some(date)),
        Cell::Null => Ok(None),
        other => Err(Error::programming(format!(
            "Supplier validity returned {other:?}, expected date or null."
        ))),
    }
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum SortValue {
    Null,
    Bool(bool),
    Number(ExactRatio),
    Text(Arc<str>),
    Date(Date),
    DateTime(DateTime),
}

fn validate_order(model: &Records, name: &str) -> Result<()> {
    let field = model.spec().field(name).ok_or_else(|| {
        Error::validation(format!(
            "Unknown seller ordering field '{name}' on product.supplierinfo."
        ))
    })?;
    if !matches!(
        field.ty,
        FieldType::Id
            | FieldType::Boolean
            | FieldType::Integer
            | FieldType::Float { .. }
            | FieldType::Monetary { .. }
            | FieldType::Char { .. }
            | FieldType::Text
            | FieldType::Date
            | FieldType::Datetime
            | FieldType::Selection { .. }
    ) {
        return Err(Error::validation(format!(
            "Unsupported seller ordering field '{name}': expected a scalar number, text, boolean or date."
        )));
    }
    Ok(())
}

fn order_value(cell: Cell, name: &str) -> Result<SortValue> {
    Ok(match cell {
        Cell::Null => SortValue::Null,
        Cell::Bool(v) => SortValue::Bool(v),
        Cell::Int(v) => SortValue::Number(ExactRatio::from_i64(i64::from(v))),
        Cell::Id(v) => SortValue::Number(ExactRatio::from_i64(v)),
        Cell::Decimal(v) => SortValue::Number(ExactRatio::from_decimal(v)?),
        Cell::Str(v) => SortValue::Text(v),
        Cell::Date(v) => SortValue::Date(v),
        Cell::DateTime(v) => SortValue::DateTime(v),
        other => {
            return Err(Error::validation(format!(
                "Unsupported value {other:?} for seller ordering field '{name}'."
            )));
        }
    })
}

async fn currency_factors(
    env: &Env,
    tx: &mut Transaction,
    company: i64,
    target: i64,
    date: Date,
    currencies: BTreeSet<i64>,
) -> Result<BTreeMap<i64, ExactRatio>> {
    let mut factors = BTreeMap::new();
    factors.insert(target, ExactRatio::from_i64(1));
    let currencies: Vec<_> = currencies.into_iter().filter(|id| *id != target).collect();
    // Respect the existing provider admission bound, reserving one slot for the target.
    for chunk in currencies.chunks(MAX_CURRENCIES_PER_CALL - 1) {
        let mut ids = Vec::with_capacity(chunk.len() + 1);
        ids.push(real(target)?);
        for id in chunk {
            ids.push(real(*id)?);
        }
        let book = RateBook::load(
            env,
            tx,
            real(company)?,
            date,
            RateDateRule::StrictlyBefore,
            &ids,
        )
        .await?;
        for id in chunk {
            factors.insert(*id, book.snapshot(real(*id)?, real(target)?)?.factor()?);
        }
    }
    Ok(factors)
}
