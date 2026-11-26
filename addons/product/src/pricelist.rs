//! Customer Pricelist models (`product.pricelist`, `product.pricelist.item`).

use crate::defaults::{company_currency_id, env_company_id, opt_id};
use crate::error::ProductError;
use crate::product::ProductProductMethods;
use erp_sdk::ValueMap;
use erp_sdk::prelude::*;

/// Any other type is a
/// programming error and is reported, not read as zero.
fn cell_decimal(c: Cell, field: &str) -> Result<Decimal> {
    match c {
        Cell::Decimal(d) => Ok(d),
        Cell::Null => Ok(Decimal::ZERO),
        other => Err(Error::programming(format!(
            "pricelist field {field} returned {other:?}, expected a decimal"
        ))),
    }
}

fn cell_id(c: Cell, field: &str) -> Result<Option<i64>> {
    opt_id(c, field)
}

fn cell_str(c: Cell, field: &str) -> Result<String> {
    match c {
        Cell::Str(s) => Ok(s.to_string()),
        Cell::Null => Ok(String::new()),
        other => Err(Error::programming(format!(
            "pricelist field {field} returned {other:?}, expected text"
        ))),
    }
}

/// `product.pricelist`: Price rule collection for customer volume discounts and currencies.
#[model(
    name = "product.pricelist",
    description = "Pricelist",
    order = "sequence, id",
    rec_name = "name",
    company_access = direct(field = "company_id", branch = "ancestor", empty = "allow"),
)]
pub struct ProductPricelist {
    /// Pricelist label.
    #[field(char, required = true, translate = true)]
    pub name: Option<Arc<str>>,

    /// Evaluation sequence.
    #[field(integer, default = 16)]
    pub sequence: i32,

    /// Target currency for price calculation.
    #[field(many2one, comodel = "res.currency", required = true)]
    pub currency_id: Option<RecId>,

    /// Restricting company.
    #[field(many2one, comodel = "res.company", index = true)]
    pub company_id: Option<RecId>,

    /// Active flag.
    #[field(boolean, default = true)]
    pub active: bool,

    /// Contained pricing rules.
    #[field(
        one2many,
        comodel = "product.pricelist.item",
        inverse_name = "pricelist_id"
    )]
    pub item_ids: Vec<RecId>,
}

#[methods(ProductPricelist, addon = "product")]
impl ProductPricelist {
    #[method(override)]
    async fn default_get(
        this: &RecordSet<ProductPricelist>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<ValueMap> {
        let mut res = sup.call(tx, this.as_records(), (names.clone(),)).await?;
        if names.iter().any(|n| n == "currency_id") && !res.contains_key("currency_id") {
            let company = env_company_id(this.env(), "the default pricelist currency")?;
            let currency = company_currency_id(tx, this.env(), company).await?;
            res.insert("currency_id".into(), Value::Int(currency));
        }
        Ok(res)
    }

    /// A pricelist without currency takes the currency of its company, or of the active company
    /// when it has none. With neither, the create is refused: there is no "first currency".
    #[method(override)]
    async fn create(
        this: &RecordSet<ProductPricelist>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        for vals in &mut vals_list {
            if !vals.contains("currency_id") {
                let company = match vals.get("company_id") {
                    Some(c) => opt_id(c.clone(), "company_id")?,
                    None => None,
                };
                let company = match company {
                    Some(id) => id,
                    None => env_company_id(
                        this.env(),
                        "the currency of a pricelist without currency or company",
                    )?,
                };
                let currency = company_currency_id(tx, this.env(), company).await?;
                vals.insert("currency_id".into(), Cell::Id(currency));
            }
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }
}

/// `product.pricelist.item`: Specific pricing directive within a pricelist.
#[model(
    name = "product.pricelist.item",
    description = "Pricelist Rule",
    order = "applied_on, min_quantity desc, date_start desc, id desc",
    company_access = parent(edge = "pricelist_id"),
)]
pub struct ProductPricelistItem {
    /// Parent pricelist.
    #[field(
        many2one,
        comodel = "product.pricelist",
        required = true,
        ondelete = "cascade",
        index = true
    )]
    pub pricelist_id: Option<RecId>,

    /// Rule application scope.
    #[field(
        selection,
        selection = [
            ("0_product_variant", "Product Variant"),
            ("1_product", "Product Template"),
            ("2_product_category", "Product Category"),
            ("3_global", "All Products")
        ],
        default = "3_global",
        required = true,
        string = "Apply On"
    )]
    pub applied_on: Option<Arc<str>>,

    /// Specific variant target (applied_on == '0_product_variant').
    #[field(many2one, comodel = "product.product", index = true)]
    pub product_id: Option<RecId>,

    /// Specific template target (applied_on == '1_product').
    #[field(many2one, comodel = "product.template", index = true)]
    pub product_tmpl_id: Option<RecId>,

    /// Specific category target (applied_on == '2_product_category').
    #[field(many2one, comodel = "product.category", index = true)]
    pub categ_id: Option<RecId>,

    /// Minimum threshold quantity.
    #[field(float, digits = 0, string = "Min. Quantity")]
    pub min_quantity: Decimal,

    /// Validity start datetime.
    #[field(datetime)]
    pub date_start: Option<DateTime>,

    /// Validity end datetime.
    #[field(datetime)]
    pub date_end: Option<DateTime>,

    /// Base price source: 'list_price', 'standard_price', 'pricelist'.
    #[field(
        selection,
        selection = [
            ("list_price", "Sales Price"),
            ("standard_price", "Cost"),
            ("pricelist", "Other Pricelist")
        ],
        default = "list_price",
        required = true,
        string = "Based on"
    )]
    pub base: Option<Arc<str>>,

    /// Source pricelist when base == 'pricelist'.
    #[field(many2one, comodel = "product.pricelist", index = true)]
    pub base_pricelist_id: Option<RecId>,

    /// Calculation method: 'fixed', 'discount', 'markup'.
    #[field(
        selection,
        selection = [
            ("fixed", "Fixed Price"),
            ("discount", "Percentage Discount"),
            ("markup", "Formula / Surcharge")
        ],
        default = "fixed",
        required = true
    )]
    pub compute_price: Option<Arc<str>>,

    /// Target fixed price.
    #[field(float, digits = "Product Price", string = "Fixed Price")]
    pub fixed_price: Decimal,

    /// Discount percentage.
    #[field(float, digits = "Discount", string = "Price Discount")]
    pub price_discount: Decimal,

    /// Markup percentage.
    #[field(float, digits = "Discount", compute = "_compute_price_markup")]
    pub price_markup: Decimal,

    /// Fixed surcharge added after discount and rounding.
    #[field(float, digits = "Product Price")]
    pub price_surcharge: Decimal,

    /// Rounding specification.
    #[field(float, digits = 0)]
    pub price_round: Decimal,

    /// Minimum margin floor above base price.
    #[field(float, digits = "Product Price")]
    pub price_min_margin: Decimal,

    /// Maximum margin ceiling above base price.
    #[field(float, digits = "Product Price")]
    pub price_max_margin: Decimal,
}

#[methods(ProductPricelistItem, addon = "product")]
impl ProductPricelistItem {
    #[compute(depends = ["price_discount"])]
    async fn _compute_price_markup(
        this: &RecordSet<ProductPricelistItem>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ProductPricelistItem>,
    ) -> Result<()> {
        let discounts = this
            .mapped(tx, ProductPricelistItem::PRICE_DISCOUNT)
            .await?;
        for (idx, discount) in discounts.into_iter().enumerate() {
            out.set(idx, ProductPricelistItem::PRICE_MARKUP, -discount)?;
        }
        Ok(())
    }

    #[method(override)]
    async fn create(
        this: &RecordSet<ProductPricelistItem>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        for vals in &mut vals_list {
            if let Some(Cell::Decimal(markup)) = vals.get("price_markup") {
                vals.insert("price_discount".into(), Cell::Decimal(-*markup));
            }
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<ProductPricelistItem>,
        tx: &mut Transaction,
        mut vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        if let Some(Cell::Decimal(markup)) = vals.get("price_markup") {
            vals.insert("price_discount".into(), Cell::Decimal(-*markup));
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }
}

type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

impl ProductPricelist {
    /// Compute price for a given product variant, quantity, and date.
    ///
    /// # Errors
    ///
    /// `User` (`ProductError::PricelistRecursion`) when a chain of `base = pricelist` rules loops
    /// back to a pricelist already being evaluated; the price is never silently replaced by the
    /// list price.
    pub async fn get_product_price(
        this: &RecordSet<ProductPricelist>,
        tx: &mut Transaction,
        product_id: i64,
        quantity: Decimal,
        date: Option<DateTime>,
    ) -> Result<Decimal> {
        let pricelist_id = Self::single_id(this)?;
        let mut visited = std::collections::BTreeSet::new();
        visited.insert(pricelist_id);
        Self::_compute_price_recursive(this, tx, product_id, quantity, date, &mut visited).await
    }

    fn single_id(this: &RecordSet<ProductPricelist>) -> Result<i64> {
        match this.as_records().rec_ids() {
            [one] => Ok(one.get()),
            other => Err(Error::programming(format!(
                "get_product_price needs exactly one pricelist, got {}",
                other.len()
            ))),
        }
    }

    /// The rule of this one pricelist that applies to `product_id` at `quantity` on `date`, the
    /// first in the pricelist's rule order; `None` when no rule applies (the list price is used).
    ///
    /// # Errors
    ///
    /// The errors of the reads; `Programming` for more than one pricelist or an unknown
    /// `applied_on`.
    pub async fn get_product_rule(
        this: &RecordSet<ProductPricelist>,
        tx: &mut Transaction,
        product_id: i64,
        quantity: Decimal,
        date: Option<DateTime>,
    ) -> Result<Option<Records>> {
        let prod_model = this.env().records("product.product")?;
        let prod = prod_model.browse(vec![product_id])?;
        let tmpl_id = cell_id(prod.cell(tx, "product_tmpl_id").await?, "product_tmpl_id")?
            .ok_or_else(|| {
                Error::validation(format!("Product variant {product_id} has no template."))
            })?;
        let tmpl_model = this.env().records("product.template")?;
        let tmpl = tmpl_model.browse(vec![tmpl_id])?;
        let categ_id = cell_id(tmpl.cell(tx, "categ_id").await?, "categ_id")?;

        let item_model = this.env().records("product.pricelist.item")?;
        let pricelist_id = Self::single_id(this)?;

        let d1 = Domain::condition("pricelist_id", Operator::Eq, pricelist_id)?;
        let items = item_model.search(tx, &d1).await?;

        let mut best_item: Option<Records> = None;
        for item_rec_id in items.rec_ids() {
            let item = item_model.browse(vec![item_rec_id.get()])?;
            let min_qty = cell_decimal(item.cell(tx, "min_quantity").await?, "min_quantity")?;
            if min_qty > quantity {
                continue;
            }

            if let Some(target_date) = date {
                if let Cell::DateTime(start) = item.cell(tx, "date_start").await?
                    && target_date < start
                {
                    continue;
                }
                if let Cell::DateTime(end) = item.cell(tx, "date_end").await?
                    && target_date > end
                {
                    continue;
                }
            }

            let applied_on = cell_str(item.cell(tx, "applied_on").await?, "applied_on")?;
            let matched = match applied_on.as_str() {
                "0_product_variant" => {
                    cell_id(item.cell(tx, "product_id").await?, "product_id")? == Some(product_id)
                }
                "1_product" => {
                    cell_id(item.cell(tx, "product_tmpl_id").await?, "product_tmpl_id")?
                        == Some(tmpl_id)
                }
                "2_product_category" => {
                    match (
                        cell_id(item.cell(tx, "categ_id").await?, "categ_id")?,
                        categ_id,
                    ) {
                        (Some(rule_cat), Some(prod_cat_id)) if rule_cat == prod_cat_id => true,
                        (Some(rule_cat), Some(prod_cat_id)) => {
                            let cat_model = this.env().records("product.category")?;
                            let prod_cat = cat_model.browse(vec![prod_cat_id])?;
                            let rule_cat_rec = cat_model.browse(vec![rule_cat])?;
                            let p_path =
                                cell_str(prod_cat.cell(tx, "parent_path").await?, "parent_path")?;
                            let r_path = cell_str(
                                rule_cat_rec.cell(tx, "parent_path").await?,
                                "parent_path",
                            )?;
                            !r_path.is_empty() && p_path.starts_with(&r_path)
                        }
                        _ => false,
                    }
                }
                "3_global" => true,
                other => {
                    return Err(Error::programming(format!(
                        "pricelist item has unknown applied_on '{other}'"
                    )));
                }
            };

            if matched {
                best_item = Some(item);
                break;
            }
        }
        Ok(best_item)
    }

    fn _compute_price_recursive<'a>(
        this: &'a RecordSet<ProductPricelist>,
        tx: &'a mut Transaction,
        product_id: i64,
        quantity: Decimal,
        date: Option<DateTime>,
        visited: &'a mut std::collections::BTreeSet<i64>,
    ) -> BoxFuture<'a, Result<Decimal>> {
        Box::pin(async move {
            let env = this.env();
            let prod = env.records("product.product")?.browse(vec![product_id])?;
            let best_item = Self::get_product_rule(this, tx, product_id, quantity, date).await?;

            let target_currency = cell_id(
                this.as_records().cell(tx, "currency_id").await?,
                "currency_id",
            )?;
            let pl_company = cell_id(
                this.as_records().cell(tx, "company_id").await?,
                "company_id",
            )?;
            let company_id = match pl_company {
                Some(c) => c,
                None => crate::defaults::env_company_id(env, "the pricelist currency conversion")?,
            };
            let company_currency = company_currency_id(tx, env, company_id).await?;
            let rate_day = match date {
                Some(dt) => Date::context_today_at(env.tz(), dt)?,
                None => env.context_today()?,
            };

            let lst_price_raw = cell_decimal(prod.cell(tx, "lst_price").await?, "lst_price")?;
            let lst_price = match target_currency {
                Some(tc) if tc != company_currency => {
                    convert_currency(
                        env,
                        tx,
                        lst_price_raw,
                        company_currency,
                        tc,
                        company_id,
                        rate_day,
                    )
                    .await?
                }
                _ => lst_price_raw,
            };

            let Some(item) = best_item else {
                return Ok(lst_price);
            };

            let rule_base = cell_str(item.cell(tx, "base").await?, "base")?;
            let base_price: Decimal = match rule_base.as_str() {
                // the cost goes through Hook N6, so a valuation addon supplies it per row
                "standard_price" => {
                    let variant = RecordSet::<crate::ProductProduct>::try_from(prod.clone())?;
                    let cost = variant
                        ._get_standard_price(tx)
                        .await?
                        .into_iter()
                        .next()
                        .ok_or_else(|| {
                            Error::programming(
                                "_get_standard_price returned no value for a variant",
                            )
                        })?;
                    match target_currency {
                        Some(tc) if tc != company_currency => {
                            convert_currency(
                                env,
                                tx,
                                cost,
                                company_currency,
                                tc,
                                company_id,
                                rate_day,
                            )
                            .await?
                        }
                        _ => cost,
                    }
                }
                "pricelist" => {
                    match cell_id(
                        item.cell(tx, "base_pricelist_id").await?,
                        "base_pricelist_id",
                    )? {
                        Some(base_pl_id) => {
                            if !visited.insert(base_pl_id) {
                                return Err(ProductError::PricelistRecursion {
                                    pricelist_id: base_pl_id,
                                }
                                .into());
                            }
                            let base_pl_model = env.records("product.pricelist")?;
                            let base_pl = base_pl_model.browse(vec![base_pl_id])?;
                            let base_pl_currency =
                                cell_id(base_pl.cell(tx, "currency_id").await?, "currency_id")?;
                            let base_pl_set = RecordSet::<ProductPricelist>::try_from(base_pl)?;
                            let raw_price = Self::_compute_price_recursive(
                                &base_pl_set,
                                tx,
                                product_id,
                                quantity,
                                date,
                                visited,
                            )
                            .await?;
                            // a sibling branch may use the same pricelist again: only a loop
                            // along the current chain is a cycle
                            visited.remove(&base_pl_id);
                            match (base_pl_currency, target_currency) {
                                (Some(src), Some(dst)) if src != dst => {
                                    convert_currency(
                                        env, tx, raw_price, src, dst, company_id, rate_day,
                                    )
                                    .await?
                                }
                                _ => raw_price,
                            }
                        }
                        None => lst_price,
                    }
                }
                "list_price" => lst_price,
                other => {
                    return Err(Error::programming(format!(
                        "pricelist item has unknown base '{other}'"
                    )));
                }
            };

            let compute_price = cell_str(item.cell(tx, "compute_price").await?, "compute_price")?;
            match compute_price.as_str() {
                "fixed" => {
                    let fixed = cell_decimal(item.cell(tx, "fixed_price").await?, "fixed_price")?;
                    let applied_on = cell_str(item.cell(tx, "applied_on").await?, "applied_on")?;
                    if applied_on == "0_product_variant" {
                        Ok(fixed)
                    } else {
                        let extra_raw =
                            cell_decimal(prod.cell(tx, "price_extra").await?, "price_extra")?;
                        let extra = match target_currency {
                            Some(tc) if tc != company_currency && !extra_raw.is_zero() => {
                                convert_currency(
                                    env,
                                    tx,
                                    extra_raw,
                                    company_currency,
                                    tc,
                                    company_id,
                                    rate_day,
                                )
                                .await?
                            }
                            _ => extra_raw,
                        };
                        Ok(fixed + extra)
                    }
                }
                "discount" | "markup" => {
                    let discount =
                        cell_decimal(item.cell(tx, "price_discount").await?, "price_discount")?;
                    let round = cell_decimal(item.cell(tx, "price_round").await?, "price_round")?;
                    let surcharge =
                        cell_decimal(item.cell(tx, "price_surcharge").await?, "price_surcharge")?;
                    let min_margin =
                        cell_decimal(item.cell(tx, "price_min_margin").await?, "price_min_margin")?;
                    let max_margin =
                        cell_decimal(item.cell(tx, "price_max_margin").await?, "price_max_margin")?;
                    Ok(apply_pricelist_formula(
                        base_price, discount, round, surcharge, min_margin, max_margin,
                    ))
                }
                other => Err(Error::programming(format!(
                    "pricelist item has unknown compute_price '{other}'"
                ))),
            }
        })
    }
}

async fn convert_currency(
    env: &Env,
    tx: &mut Transaction,
    amount: Decimal,
    from_currency: i64,
    to_currency: i64,
    company: i64,
    date: Date,
) -> Result<Decimal> {
    if from_currency == to_currency || amount.is_zero() {
        return Ok(amount);
    }
    let from = RecId::from_real(from_currency)
        .ok_or_else(|| Error::invariant("Invalid source currency id."))?;
    let to = RecId::from_real(to_currency)
        .ok_or_else(|| Error::invariant("Invalid target currency id."))?;
    let comp = RecId::from_real(company).ok_or_else(|| Error::invariant("Invalid company id."))?;
    erp_addon_base::money::convert(env, tx, amount, from, to, comp, date).await
}

/// Pure exact-decimal pricelist formula: discount/markup, rounding multiple, surcharge,
/// and min/max margin clamping.
pub fn apply_pricelist_formula(
    base_price: Decimal,
    discount_pct: Decimal,
    price_round: Decimal,
    price_surcharge: Decimal,
    min_margin: Decimal,
    max_margin: Decimal,
) -> Decimal {
    let mut price = base_price - (base_price * (discount_pct / Decimal::from(100)));
    if price_round > Decimal::ZERO {
        price = (price / price_round)
            .round_dp_with_strategy(0, rust_decimal::RoundingStrategy::MidpointAwayFromZero)
            * price_round;
    }
    price += price_surcharge;
    if min_margin > Decimal::ZERO && price < base_price + min_margin {
        price = base_price + min_margin;
    }
    if max_margin > Decimal::ZERO && price > base_price + max_margin {
        price = base_price + max_margin;
    }
    price
}

#[cfg(test)]
mod pricelist_formula_tests {
    use super::apply_pricelist_formula;
    use erp_sdk::Decimal;
    use std::str::FromStr;

    #[test]
    fn exact_pricelist_formula_applies_discount_rounding_and_surcharge() {
        let base = Decimal::from_str("100.00").unwrap();
        let price = apply_pricelist_formula(
            base,
            Decimal::from_str("15").unwrap(),
            Decimal::from_str("1").unwrap(),
            Decimal::from_str("-0.01").unwrap(),
            Decimal::ZERO,
            Decimal::ZERO,
        );
        assert_eq!(price, Decimal::from_str("84.99").unwrap());
    }
}
