// SPDX-License-Identifier: MIT OR Apache-2.0
//! Prices, taxes and amounts of sales order lines and the totals of their orders (B3;
//! reference and
//! ).
//!
//! A new product line without a price takes it from the order's pricelist (the pricelist
//! machinery of `product`), else from the product's sales price converted to the order currency,
//! in the line's unit. Its taxes are the product's sales taxes of the order company, or the
//! company's default sales tax for a line without product, mapped by the order's fiscal position.
//! Subtotal, tax and total come from the tax computation of `account`
//! (`erp_addon_account::tax::compute`), rounded per line (the company's "round globally" choice
//! comes with [`ROUNDING_TASK`](erp_addon_account::tax::compute::ROUNDING_TASK)); a group of
//! taxes and a reverse-charge tax are refused by name, as on invoices. The derived amounts are
//! never written by a client.

use std::collections::{BTreeMap, BTreeSet};

use erp_addon_account::tax::compute::{self, AmountType, BaseLine, TaxDef, TaxKind};
use erp_addon_base::money::{CURRENCY_ROUNDING_MODE, RoundingTable, convert};
use erp_addon_product::ProductPricelist;
use erp_addon_uom::conversion::price_exact;
use erp_addon_uom::models::load_unit;
use erp_sdk::Command;
use erp_sdk::money::{Increment, RoundingMode};
use erp_sdk::prelude::*;

/// Fields of a line only this module writes; `currency_id` is the order's currency.
pub(crate) const LINE_DERIVED: [&str; 5] = [
    "price_subtotal",
    "price_tax",
    "price_total",
    "technical_price_unit",
    "currency_id",
];

/// [`LINE_DERIVED`] of a new line: the line form sends `technical_price_unit` with its untouched
/// default price, which tells that price from a typed one ([`fill_line`]).
pub(crate) const LINE_DERIVED_ON_CREATE: [&str; 4] =
    ["price_subtotal", "price_tax", "price_total", "currency_id"];

/// Fields of an order only this module and its computes write.
pub(crate) const ORDER_DERIVED: [&str; 5] = [
    "amount_untaxed",
    "amount_tax",
    "amount_total",
    "currency_id",
    "currency_rate",
];

/// Refuses a client value of `fields` of `model`, given with the record or as a `default_<field>`
/// key of the context (which the base create applies after this check).
pub(crate) fn refuse_derived(
    tx: &Transaction,
    env: &Env,
    vals: &RawVals,
    model: &str,
    fields: &[&str],
) -> Result<()> {
    for field in fields {
        let given = vals.contains(field) || vals.commands(field).is_some();
        let defaulted = env.context().contains(&format!("default_{field}"));
        if given || defaulted {
            let label = crate::order::label_of(tx, model, field)?;
            let via = if given {
                String::new()
            } else {
                format!(" (context key default_{field})")
            };
            return Err(Error::validation(format!(
                "The field \"{label}\"{via} is computed from the order and its lines and cannot be written."
            )));
        }
    }
    Ok(())
}

pub(crate) fn id_of(cell: &Cell, field: &str) -> Result<Option<i64>> {
    match cell {
        Cell::Null | Cell::Bool(false) => Ok(None),
        Cell::Id(id) => Ok(RecId::from_raw(*id)
            .and_then(|rec| rec.as_real().or(rec.origin()))
            .map(|rec| rec.get())),
        Cell::Int(id) => Ok(Some(i64::from(*id))),
        other => Err(Error::validation(format!(
            "{field} takes a record id, not {other:?}."
        ))),
    }
}

fn number(cell: &Cell, field: &str) -> Result<Decimal> {
    match cell {
        Cell::Null | Cell::Bool(false) => Ok(Decimal::ZERO),
        Cell::Decimal(value) => Ok(*value),
        Cell::Int(value) => Ok(Decimal::from(*value)),
        Cell::Float(value) => Decimal::try_from(*value)
            .map_err(|e| Error::validation(format!("{field} {value} is not exact: {e}"))),
        other => Err(Error::validation(format!(
            "{field} takes a number, not {other:?}."
        ))),
    }
}

fn ids_of(cell: &Cell) -> Vec<i64> {
    match cell {
        Cell::Ids(ids) => ids
            .iter()
            .filter_map(|id| id.as_real().or(id.origin()).map(|id| id.get()))
            .collect(),
        _ => Vec::new(),
    }
}

fn rec(id: i64) -> Result<RecId> {
    RecId::from_real(id).ok_or_else(|| Error::invariant(format!("record id {id}")))
}

fn sum(left: Decimal, right: Decimal) -> Result<Decimal> {
    left.checked_add(right)
        .ok_or_else(|| Error::validation("A sales order amount is out of range."))
}

/// The day of `at` in the user's time zone: the one date every price and rate of an order uses.
pub(crate) fn order_day(env: &Env, at: DateTime) -> Result<Date> {
    Date::context_today_at(env.tz(), at).map_err(|e| Error::internal(e.message().to_owned()))
}

/// What a line reads of its order.
pub(crate) struct Head {
    company: i64,
    currency: i64,
    pricelist: Option<i64>,
    fiscal_position: Option<i64>,
    date_order: DateTime,
}

/// The heads of `ids`, read as the caller (who reads the orders of the lines they change).
pub(crate) async fn heads(
    tx: &mut Transaction,
    env: &Env,
    ids: &BTreeSet<i64>,
) -> Result<BTreeMap<i64, Head>> {
    let orders = env.records("sale.order")?.browse(ids.iter().copied())?;
    let mut columns = BTreeMap::new();
    for field in [
        "company_id",
        "currency_id",
        "pricelist_id",
        "fiscal_position_id",
        "date_order",
    ] {
        columns.insert(field, orders.cells(tx, field).await?);
    }
    let mut out = BTreeMap::new();
    for (i, id) in orders.ids().into_iter().enumerate() {
        let cell = |field: &str| -> Result<&Cell> {
            columns
                .get(field)
                .and_then(|c| c.get(i))
                .ok_or_else(|| Error::invariant(format!("order column {field} is short")))
        };
        let required = |field: &str| -> Result<i64> {
            id_of(cell(field)?, field)?
                .ok_or_else(|| Error::invariant(format!("sales order {id} has no {field}")))
        };
        out.insert(
            id,
            Head {
                company: required("company_id")?,
                currency: required("currency_id")?,
                pricelist: id_of(cell("pricelist_id")?, "pricelist_id")?,
                fiscal_position: id_of(cell("fiscal_position_id")?, "fiscal_position_id")?,
                date_order: match cell("date_order")? {
                    Cell::DateTime(at) => *at,
                    other => {
                        return Err(Error::invariant(format!(
                            "sales order {id} order date is {other:?}"
                        )));
                    }
                },
            },
        );
    }
    Ok(out)
}

/// The unit price of `product` in `unit` for `quantity` on an order with `head`: the pricelist's
/// price, else the product's sales price converted from the company currency at the order day.
///
/// A pricelist in another currency than the company's prices only through a fixed-price rule (its
/// price is in the pricelist currency); any other price would need the sales price converted
/// inside the pricelist, which is not built yet, and is refused by name instead of taken
/// unconverted.
pub(crate) async fn price_of(
    tx: &mut Transaction,
    env: &Env,
    head: &Head,
    product: i64,
    quantity: Decimal,
    unit: i64,
) -> Result<Decimal> {
    let company_env = env.with_company(head.company)?;
    let record = company_env.records("product.product")?.browse([product])?;
    let company_currency = id_of(
        &company_env
            .records("res.company")?
            .browse([head.company])?
            .cell(tx, "currency_id")
            .await?,
        "currency_id",
    )?
    .ok_or_else(|| Error::invariant(format!("company {} has no currency", head.company)))?;
    // a pricelist rule's minimum is in the product's unit: the line's quantity is converted first
    let product_unit = id_of(&record.cell(tx, "uom_id").await?, "uom_id")?
        .ok_or_else(|| Error::invariant(format!("product {product} has no unit")))?;
    let source = load_unit(&company_env, tx, product_unit).await?;
    let target = load_unit(&company_env, tx, unit).await?;
    // rounded at the Product Unit precision, as a quantity in another unit always is
    let unit_digits =
        erp_addon_base::decimal_precision::precision_get(&company_env, tx, "Product Unit").await?;
    let quantity = erp_addon_uom::conversion::quantity_rounded(
        &target,
        &source,
        quantity,
        &Increment::from_places(unit_digits)?,
        RoundingMode::AwayFromZero,
    )?;
    let price = match head.pricelist {
        Some(pricelist) => {
            let pl_rec = company_env
                .records("product.pricelist")?
                .browse([pricelist])?;
            let pl_currency = id_of(&pl_rec.cell(tx, "currency_id").await?, "currency_id")?;
            let set = RecordSet::<ProductPricelist>::try_from(pl_rec)?;
            let raw = ProductPricelist::get_product_price(
                &set,
                tx,
                product,
                quantity,
                Some(head.date_order),
            )
            .await?;
            match pl_currency {
                Some(pc) if pc != head.currency => {
                    convert(
                        &company_env,
                        tx,
                        raw,
                        rec(pc)?,
                        rec(head.currency)?,
                        rec(head.company)?,
                        order_day(env, head.date_order)?,
                    )
                    .await?
                }
                _ => raw,
            }
        }
        None => {
            let list = number(&record.cell(tx, "lst_price").await?, "lst_price")?;
            if company_currency == head.currency {
                list
            } else {
                convert(
                    &company_env,
                    tx,
                    list,
                    rec(company_currency)?,
                    rec(head.currency)?,
                    rec(head.company)?,
                    order_day(env, head.date_order)?,
                )
                .await?
            }
        }
    };
    let digits =
        erp_addon_base::decimal_precision::precision_get(&company_env, tx, "Product Price").await?;
    let precision = Increment::from_places(digits)?;
    Ok(price_exact(&source, &target, price)?.quantize(&precision, CURRENCY_ROUNDING_MODE)?)
}

/// The taxes of a line: the product's sales taxes of the order company (or a parent), or for a
/// line without product and without display type the company's default sales tax; mapped by the
/// order's fiscal position.
pub(crate) async fn taxes_of(
    tx: &mut Transaction,
    env: &Env,
    head: &Head,
    product: Option<i64>,
) -> Result<Vec<RecId>> {
    let company_env = env.with_company(head.company)?;
    let proposed = match product {
        Some(product) => ids_of(
            &company_env
                .records("product.product")?
                .browse([product])?
                .cell(tx, "taxes_id")
                .await?,
        ),
        None => id_of(
            &company_env
                .records("res.company")?
                .browse([head.company])?
                .cell(tx, "account_sale_tax_id")
                .await?,
            "account_sale_tax_id",
        )?
        .into_iter()
        .collect(),
    };
    if proposed.is_empty() {
        return Ok(Vec::new());
    }
    let mut taxes = company_env
        .records("account.tax")?
        .search(
            tx,
            &Domain::and([
                Domain::condition("id", Operator::In, proposed)?,
                Domain::condition("company_id", Operator::ParentOf, head.company)?,
            ]),
        )
        .await?
        .ids();
    if let Some(position) = head.fiscal_position {
        taxes = company_env
            .records("account.fiscal.position")?
            .browse([position])?
            .call(tx, "map_tax", (taxes,))
            .await?;
    }
    taxes.into_iter().map(rec).collect()
}

/// Price and taxes of a new line when not given; the given price counts as the last price the line
/// was priced at. A section or note line takes neither.
pub(crate) async fn fill_line(
    tx: &mut Transaction,
    env: &Env,
    head: &Head,
    vals: &mut RawVals,
) -> Result<()> {
    if matches!(vals.get("display_type"), Some(Cell::Str(_))) {
        return Ok(());
    }
    let product = crate::order::given_id(env, vals, "product_id")?.map(|id| id.get());
    // only an absent key takes the product's taxes: a given set, the empty one too, is kept
    if !vals.contains("tax_ids") && vals.commands("tax_ids").is_none() {
        let taxes = taxes_of(tx, env, head, product).await?;
        vals.insert_commands("tax_ids".into(), vec![Command::Set(taxes)]);
    }
    // a price is typed unless it equals the technical price sent with it: the line form sends
    // its untouched default price with the technical default 0;
    // a price given without it is the caller's
    let typed = match (vals.get("price_unit"), vals.get("technical_price_unit")) {
        (None, _) => false,
        (Some(price), Some(last)) => {
            number(price, "price_unit")? != number(last, "technical_price_unit")?
        }
        (Some(_), None) => true,
    };
    if let Some(product) = product
        && !typed
    {
        let unit = match vals.get("product_uom_id") {
            Some(cell) => id_of(cell, "product_uom_id")?,
            None => None,
        };
        let unit = match unit {
            Some(unit) => unit,
            None => {
                let unit = id_of(
                    &env.with_company(head.company)?
                        .records("product.product")?
                        .browse([product])?
                        .cell(tx, "uom_id")
                        .await?,
                    "uom_id",
                )?
                .ok_or_else(|| Error::invariant(format!("product {product} has no unit")))?;
                vals.insert("product_uom_id".into(), Cell::Id(unit));
                unit
            }
        };
        let quantity = match vals.get("product_uom_qty") {
            Some(cell) => number(cell, "product_uom_qty")?,
            None => Decimal::ONE,
        };
        let price = price_of(tx, env, head, product, quantity, unit).await?;
        vals.insert("price_unit".into(), Cell::Decimal(price));
        vals.insert("technical_price_unit".into(), Cell::Decimal(price));
    }
    // a typed price keeps the technical price it came with, so it stays a price set by hand
    if !vals.contains("technical_price_unit") {
        let price = vals
            .get("price_unit")
            .cloned()
            .unwrap_or(Cell::Decimal(Decimal::ZERO));
        vals.insert("technical_price_unit".into(), price);
    }
    Ok(())
}

/// Lines whose unit price was set by a person: it differs from the price they were last priced at.
pub(crate) async fn manual_prices(tx: &mut Transaction, lines: &Records) -> Result<BTreeSet<i64>> {
    let prices = lines.cells(tx, "price_unit").await?;
    let technical = lines.cells(tx, "technical_price_unit").await?;
    let mut manual = BTreeSet::new();
    for (i, id) in lines.ids().into_iter().enumerate() {
        let get = |cells: &[Cell]| cells.get(i).cloned().unwrap_or(Cell::Null);
        if number(&get(&prices), "price_unit")? != number(&get(&technical), "technical_price_unit")?
        {
            manual.insert(id);
        }
    }
    Ok(manual)
}

/// After a change of product, unit or quantity: when `price`, the price of each line again unless
/// it was set by hand; when `retax`, the taxes again after a new product unless they were given;
/// lines with an invoiced quantity or down payments keep their price.
pub(crate) async fn reprice(
    tx: &mut Transaction,
    env: &Env,
    lines: &Records,
    manual: &BTreeSet<i64>,
    price: bool,
    retax: bool,
) -> Result<()> {
    let mut columns = BTreeMap::new();
    for field in [
        "order_id",
        "product_id",
        "product_uom_id",
        "product_uom_qty",
        "qty_invoiced",
        "is_downpayment",
        "display_type",
    ] {
        columns.insert(field, lines.cells(tx, field).await?);
    }
    let get = |field: &str, i: usize| -> Cell {
        columns
            .get(field)
            .and_then(|c| c.get(i))
            .cloned()
            .unwrap_or(Cell::Null)
    };
    let ids = lines.ids();
    let mut orders = BTreeSet::new();
    for i in 0..ids.len() {
        if let Some(order) = id_of(&get("order_id", i), "order_id")? {
            orders.insert(order);
        }
    }
    let heads = heads(tx, env, &orders).await?;
    for (i, id) in ids.iter().enumerate() {
        if matches!(get("display_type", i), Cell::Str(_)) {
            continue;
        }
        let order = id_of(&get("order_id", i), "order_id")?
            .ok_or_else(|| Error::invariant(format!("sales order line {id} has no order")))?;
        let head = heads
            .get(&order)
            .ok_or_else(|| Error::invariant(format!("sales order {order} was not read")))?;
        let product = id_of(&get("product_id", i), "product_id")?;
        let mut vals = RawVals::new();
        if retax {
            vals.insert_commands(
                "tax_ids".into(),
                vec![Command::Set(taxes_of(tx, env, head, product).await?)],
            );
        }
        let invoiced = number(&get("qty_invoiced", i), "qty_invoiced")? > Decimal::ZERO;
        let keeps = !price
            || manual.contains(id)
            || invoiced
            || matches!(get("is_downpayment", i), Cell::Bool(true));
        if !keeps
            && let (Some(product), Some(unit)) =
                (product, id_of(&get("product_uom_id", i), "product_uom_id")?)
        {
            let quantity = number(&get("product_uom_qty", i), "product_uom_qty")?;
            let price = price_of(tx, env, head, product, quantity, unit).await?;
            vals.insert("price_unit".into(), Cell::Decimal(price));
            vals.insert("technical_price_unit".into(), Cell::Decimal(price));
        }
        if !vals.is_empty() {
            env.records("sale.order.line")?
                .browse([*id])?
                .write_base(tx, vals)
                .await?;
        }
    }
    Ok(())
}

/// The taxes of `ids` as the tax computation takes them; a group of taxes and a reverse-charge tax
/// are refused by name.
async fn load_taxes(
    tx: &mut Transaction,
    env: &Env,
    ids: &BTreeSet<i64>,
) -> Result<BTreeMap<i64, TaxDef>> {
    if ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let taxes = env.records("account.tax")?.browse(ids.iter().copied())?;
    let mut columns = BTreeMap::new();
    for field in [
        "name",
        "amount_type",
        "amount",
        "sequence",
        "price_include",
        "include_base_amount",
        "is_base_affected",
        "has_negative_factor",
    ] {
        columns.insert(field, taxes.cells(tx, field).await?);
    }
    let mut defs = BTreeMap::new();
    for (i, id) in taxes.ids().into_iter().enumerate() {
        let cell = |field: &str| -> Result<Cell> {
            columns
                .get(field)
                .and_then(|c| c.get(i))
                .cloned()
                .ok_or_else(|| Error::invariant(format!("tax column {field} is short")))
        };
        let flag = |field: &str| -> Result<bool> { Ok(matches!(cell(field)?, Cell::Bool(true))) };
        let name = match cell("name")? {
            Cell::Str(name) => name.to_string(),
            _ => format!("#{id}"),
        };
        let kind = match cell("amount_type")? {
            Cell::Str(kind) if kind.as_ref() == "group" => {
                return Err(Error::user(format!(
                    "Tax {name} is a group of taxes: groups need their child taxes, which are not \
                     supported on sales order lines yet."
                )));
            }
            Cell::Str(kind) => AmountType::parse(&kind)?,
            other => return Err(Error::invariant(format!("tax {id} amount type {other:?}"))),
        };
        if flag("has_negative_factor")? {
            return Err(Error::user(format!(
                "Tax {name} has a negative distribution (reverse charge), which is not \
                 supported on sales order lines yet."
            )));
        }
        let sequence = match cell("sequence")? {
            Cell::Int(sequence) => sequence,
            other => return Err(Error::invariant(format!("tax {id} sequence is {other:?}"))),
        };
        defs.insert(
            id,
            TaxDef {
                id,
                sequence,
                kind: TaxKind::Simple(kind),
                amount: number(&cell("amount")?, "amount")?,
                price_include: flag("price_include")?,
                include_base_amount: flag("include_base_amount")?,
                is_base_affected: flag("is_base_affected")?,
            },
        );
    }
    Ok(defs)
}

/// Writes subtotal, tax and total of `lines`, then the totals of their orders.
/// Rounded per line; see
/// [`ROUNDING_TASK`](erp_addon_account::tax::compute::ROUNDING_TASK).
pub(crate) async fn sync_lines(tx: &mut Transaction, env: &Env, lines: &Records) -> Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    let lines = env.records("sale.order.line")?.browse(lines.ids())?;
    let mut columns = BTreeMap::new();
    for field in [
        "order_id",
        "price_unit",
        "product_uom_qty",
        "discount",
        "tax_ids",
    ] {
        columns.insert(field, lines.cells(tx, field).await?);
    }
    let get = |field: &str, i: usize| -> Cell {
        columns
            .get(field)
            .and_then(|c| c.get(i))
            .cloned()
            .unwrap_or(Cell::Null)
    };
    let ids = lines.ids();
    let mut tax_ids = BTreeSet::new();
    let mut orders = BTreeSet::new();
    for i in 0..ids.len() {
        tax_ids.extend(ids_of(&get("tax_ids", i)));
        if let Some(order) = id_of(&get("order_id", i), "order_id")? {
            orders.insert(order);
        }
    }
    let heads = heads(tx, env, &orders).await?;
    let defs = load_taxes(tx, env, &tax_ids).await?;
    let currencies: Vec<RecId> = heads
        .values()
        .map(|head| rec(head.currency))
        .collect::<Result<BTreeSet<_>>>()?
        .into_iter()
        .collect();
    let table = RoundingTable::load(env, tx, &currencies).await?;
    for (i, id) in ids.iter().enumerate() {
        let order = id_of(&get("order_id", i), "order_id")?
            .ok_or_else(|| Error::invariant(format!("sales order line {id} has no order")))?;
        let head = heads
            .get(&order)
            .ok_or_else(|| Error::invariant(format!("sales order {order} was not read")))?;
        let taxes: Vec<TaxDef> = ids_of(&get("tax_ids", i))
            .into_iter()
            .map(|t| {
                defs.get(&t).cloned().ok_or_else(|| {
                    Error::invariant(format!("tax {t} of sales order line {id} was not loaded"))
                })
            })
            .collect::<Result<_>>()?;
        let result = compute::compute(&BaseLine {
            price_unit: number(&get("price_unit", i), "price_unit")?,
            quantity: number(&get("product_uom_qty", i), "product_uom_qty")?,
            discount: number(&get("discount", i), "discount")?,
            taxes: &taxes,
            increment: table.get(rec(head.currency)?)?.increment(),
        })?;
        let tax = result
            .total_included
            .checked_sub(result.total_excluded)
            .ok_or_else(|| Error::validation("A sales order line amount is out of range."))?;
        env.records("sale.order.line")?
            .browse([*id])?
            .write_base(
                tx,
                RawVals::new()
                    .set_cell("price_subtotal", Cell::Decimal(result.total_excluded))
                    .set_cell("price_tax", Cell::Decimal(tax))
                    .set_cell("price_total", Cell::Decimal(result.total_included)),
            )
            .await?;
    }
    sync_orders(tx, env, &orders).await
}

/// Writes untaxed amount, taxes and total of `orders` from their lines;
/// section and note lines carry no amount.
pub(crate) async fn sync_orders(
    tx: &mut Transaction,
    env: &Env,
    orders: &BTreeSet<i64>,
) -> Result<()> {
    if orders.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = orders.iter().copied().collect();
    let lines = env
        .records("sale.order.line")?
        .search(
            tx,
            &Domain::condition("order_id", Operator::In, ids.clone())?,
        )
        .await?;
    let owners = lines.cells(tx, "order_id").await?;
    let subtotals = lines.cells(tx, "price_subtotal").await?;
    let taxes = lines.cells(tx, "price_tax").await?;
    let mut sums: BTreeMap<i64, (Decimal, Decimal)> = ids
        .iter()
        .map(|id| (*id, (Decimal::ZERO, Decimal::ZERO)))
        .collect();
    for (i, owner) in owners.iter().enumerate() {
        let Some(order) = id_of(owner, "order_id")? else {
            continue;
        };
        let entry = sums
            .get_mut(&order)
            .ok_or_else(|| Error::invariant(format!("line of order {order} outside the set")))?;
        entry.0 = sum(
            entry.0,
            number(subtotals.get(i).unwrap_or(&Cell::Null), "price_subtotal")?,
        )?;
        entry.1 = sum(
            entry.1,
            number(taxes.get(i).unwrap_or(&Cell::Null), "price_tax")?,
        )?;
    }
    for (id, (untaxed, tax)) in sums {
        env.records("sale.order")?
            .browse([id])?
            .write_base(
                tx,
                RawVals::new()
                    .set_cell("amount_untaxed", Cell::Decimal(untaxed))
                    .set_cell("amount_tax", Cell::Decimal(tax))
                    .set_cell("amount_total", Cell::Decimal(sum(untaxed, tax)?)),
            )
            .await?;
    }
    Ok(())
}

/// The lines of `orders` computed again (after a change of their currency).
pub(crate) async fn resync_orders(
    tx: &mut Transaction,
    env: &Env,
    orders: &BTreeSet<i64>,
) -> Result<()> {
    let ids: Vec<i64> = orders.iter().copied().collect();
    let lines = env
        .records("sale.order.line")?
        .search(tx, &Domain::condition("order_id", Operator::In, ids)?)
        .await?;
    if lines.is_empty() {
        return sync_orders(tx, env, orders).await;
    }
    sync_lines(tx, env, &lines).await
}

/// Read the header context of an in-memory or persisted singleton `sale.order` record for live
/// `onchange` calculations. Falls back to the active company and its currency when the parent has
/// not yet populated them.
pub(crate) async fn head_for_onchange(
    tx: &mut Transaction,
    env: &Env,
    order: &Records,
) -> Result<Option<Head>> {
    let company = if order.is_empty() {
        env.company_id()?
    } else {
        match id_of(&order.cell(tx, "company_id").await?, "company_id")? {
            Some(c) => Some(c),
            None => env.company_id()?,
        }
    };
    let Some(company) = company else {
        return Ok(None);
    };
    let pricelist = if order.is_empty() {
        None
    } else {
        id_of(&order.cell(tx, "pricelist_id").await?, "pricelist_id")?
    };
    let fiscal_position = if order.is_empty() {
        None
    } else {
        id_of(
            &order.cell(tx, "fiscal_position_id").await?,
            "fiscal_position_id",
        )?
    };
    let date_order = if order.is_empty() {
        DateTime::now()
    } else {
        match order.cell(tx, "date_order").await? {
            Cell::DateTime(at) => at,
            _ => DateTime::now(),
        }
    };
    let currency = if order.is_empty() {
        None
    } else {
        id_of(&order.cell(tx, "currency_id").await?, "currency_id")?
    };
    let currency = match currency {
        Some(c) => Some(c),
        None => id_of(
            &env.records("res.company")?
                .browse([company])?
                .cell(tx, "currency_id")
                .await?,
            "currency_id",
        )?,
    };
    let Some(currency) = currency else {
        return Ok(None);
    };
    Ok(Some(Head {
        company,
        currency,
        pricelist,
        fiscal_position,
        date_order,
    }))
}

/// Compute `(price_subtotal, price_tax, price_total)` of one line in memory (no DB write).
pub(crate) async fn line_amounts(
    tx: &mut Transaction,
    env: &Env,
    currency: i64,
    price_unit: Decimal,
    quantity: Decimal,
    discount: Decimal,
    tax_ids: &[i64],
) -> Result<(Decimal, Decimal, Decimal)> {
    let tax_set: BTreeSet<i64> = tax_ids.iter().copied().collect();
    let defs = load_taxes(tx, env, &tax_set).await?;
    let table = RoundingTable::load(env, tx, &[rec(currency)?]).await?;
    let taxes: Vec<TaxDef> = tax_ids
        .iter()
        .map(|t| {
            defs.get(t)
                .cloned()
                .ok_or_else(|| Error::invariant(format!("tax {t} was not loaded")))
        })
        .collect::<Result<_>>()?;
    let result = compute::compute(&BaseLine {
        price_unit,
        quantity,
        discount,
        taxes: &taxes,
        increment: table.get(rec(currency)?)?.increment(),
    })?;
    let tax = result
        .total_included
        .checked_sub(result.total_excluded)
        .ok_or_else(|| Error::validation("A sales order line amount is out of range."))?;
    Ok((result.total_excluded, tax, result.total_included))
}

/// Update `price_subtotal`, `price_tax`, `price_total` on an in-memory `sale.order.line` record.
pub(crate) async fn preview_line_amounts(
    tx: &mut Transaction,
    line: &Records,
    head: &Head,
) -> Result<(Decimal, Decimal, Decimal)> {
    if matches!(line.cell(tx, "display_type").await?, Cell::Str(_)) {
        line.write_base(
            tx,
            RawVals::new()
                .set_cell("price_subtotal", Cell::Decimal(Decimal::ZERO))
                .set_cell("price_tax", Cell::Decimal(Decimal::ZERO))
                .set_cell("price_total", Cell::Decimal(Decimal::ZERO)),
        )
        .await?;
        return Ok((Decimal::ZERO, Decimal::ZERO, Decimal::ZERO));
    }
    let price_unit = number(&line.cell(tx, "price_unit").await?, "price_unit")?;
    let quantity = number(&line.cell(tx, "product_uom_qty").await?, "product_uom_qty")?;
    let discount = number(&line.cell(tx, "discount").await?, "discount")?;
    let tax_ids = ids_of(&line.cell(tx, "tax_ids").await?);
    let (subtotal, tax, total) = line_amounts(
        tx,
        line.env(),
        head.currency,
        price_unit,
        quantity,
        discount,
        &tax_ids,
    )
    .await?;
    line.write_base(
        tx,
        RawVals::new()
            .set_cell("price_subtotal", Cell::Decimal(subtotal))
            .set_cell("price_tax", Cell::Decimal(tax))
            .set_cell("price_total", Cell::Decimal(total)),
    )
    .await?;
    Ok((subtotal, tax, total))
}

/// Live `onchange` for a `sale.order.line` when its product changes: proposes taxes and pricelist
/// unit price, then updates line amounts in memory.
pub(crate) async fn onchange_line_product(
    tx: &mut Transaction,
    line: &Records,
) -> Result<OnchangeResult> {
    let order = line.related(tx, "order_id").await?;
    let Some(head) = head_for_onchange(tx, line.env(), &order).await? else {
        return Ok(OnchangeResult::none());
    };
    if matches!(line.cell(tx, "display_type").await?, Cell::Str(_)) {
        preview_line_amounts(tx, line, &head).await?;
        return Ok(OnchangeResult::none());
    }
    let product = id_of(&line.cell(tx, "product_id").await?, "product_id")?;
    let taxes = taxes_of(tx, line.env(), &head, product).await?;
    let mut assign = RawVals::new();
    assign.insert_commands("tax_ids".into(), vec![Command::Set(taxes)]);
    if let Some(product) = product {
        let unit = match id_of(&line.cell(tx, "product_uom_id").await?, "product_uom_id")? {
            Some(u) => Some(u),
            None => id_of(
                &line
                    .env()
                    .with_company(head.company)?
                    .records("product.product")?
                    .browse([product])?
                    .cell(tx, "uom_id")
                    .await?,
                "uom_id",
            )?,
        };
        if let Some(unit) = unit {
            let quantity = number(&line.cell(tx, "product_uom_qty").await?, "product_uom_qty")?;
            let price = price_of(tx, line.env(), &head, product, quantity, unit).await?;
            assign.insert("price_unit".into(), Cell::Decimal(price));
            assign.insert("technical_price_unit".into(), Cell::Decimal(price));
        }
    }
    line.write_base(tx, assign).await?;
    preview_line_amounts(tx, line, &head).await?;
    Ok(OnchangeResult::none())
}

/// Live `onchange` for a `sale.order.line` when quantity or UoM changes: reprices when the price
/// was not set by hand, then updates line amounts in memory.
pub(crate) async fn onchange_line_qty_uom(
    tx: &mut Transaction,
    line: &Records,
) -> Result<OnchangeResult> {
    let order = line.related(tx, "order_id").await?;
    let Some(head) = head_for_onchange(tx, line.env(), &order).await? else {
        return Ok(OnchangeResult::none());
    };
    if matches!(line.cell(tx, "display_type").await?, Cell::Str(_)) {
        preview_line_amounts(tx, line, &head).await?;
        return Ok(OnchangeResult::none());
    }
    let price_unit = number(&line.cell(tx, "price_unit").await?, "price_unit")?;
    let technical = number(
        &line.cell(tx, "technical_price_unit").await?,
        "technical_price_unit",
    )?;
    let invoiced = number(&line.cell(tx, "qty_invoiced").await?, "qty_invoiced")? > Decimal::ZERO;
    let downpayment = matches!(line.cell(tx, "is_downpayment").await?, Cell::Bool(true));
    if price_unit == technical
        && !invoiced
        && !downpayment
        && let (Some(product), Some(unit)) = (
            id_of(&line.cell(tx, "product_id").await?, "product_id")?,
            id_of(&line.cell(tx, "product_uom_id").await?, "product_uom_id")?,
        )
    {
        let quantity = number(&line.cell(tx, "product_uom_qty").await?, "product_uom_qty")?;
        let price = price_of(tx, line.env(), &head, product, quantity, unit).await?;
        line.write_base(
            tx,
            RawVals::new()
                .set_cell("price_unit", Cell::Decimal(price))
                .set_cell("technical_price_unit", Cell::Decimal(price)),
        )
        .await?;
    }
    preview_line_amounts(tx, line, &head).await?;
    Ok(OnchangeResult::none())
}

/// Live `onchange` for a `sale.order.line` when price, discount, taxes or display type changes.
pub(crate) async fn onchange_line_amounts(
    tx: &mut Transaction,
    line: &Records,
) -> Result<OnchangeResult> {
    let order = line.related(tx, "order_id").await?;
    let Some(head) = head_for_onchange(tx, line.env(), &order).await? else {
        return Ok(OnchangeResult::none());
    };
    preview_line_amounts(tx, line, &head).await?;
    Ok(OnchangeResult::none())
}

/// Live `onchange` for `sale.order`: recomputes in-memory line subtotals and header totals
/// (`amount_untaxed`, `amount_tax`, `amount_total`) before Save.
pub(crate) async fn onchange_order_totals(
    tx: &mut Transaction,
    order: &Records,
) -> Result<OnchangeResult> {
    let Some(head) = head_for_onchange(tx, order.env(), order).await? else {
        return Ok(OnchangeResult::none());
    };
    let lines = order.related(tx, "order_line").await?;
    let mut untaxed = Decimal::ZERO;
    let mut tax_total = Decimal::ZERO;
    for line in lines.iter() {
        let (sub, tax, _) = preview_line_amounts(tx, &line, &head).await?;
        untaxed = sum(untaxed, sub)?;
        tax_total = sum(tax_total, tax)?;
    }
    let total = sum(untaxed, tax_total)?;
    order
        .write_base(
            tx,
            RawVals::new()
                .set_cell("amount_untaxed", Cell::Decimal(untaxed))
                .set_cell("amount_tax", Cell::Decimal(tax_total))
                .set_cell("amount_total", Cell::Decimal(total)),
        )
        .await?;
    Ok(OnchangeResult::none())
}
