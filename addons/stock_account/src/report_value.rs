// SPDX-License-Identifier: MIT OR Apache-2.0
//! The Value column of the dynamic Stock Report (`stock.report_stock_position`, design
//! ): a registered report function.
//!
//! Observed: a quant is worth its share
//! of the total value of its product (of its lot for lot-valued products), counted only in a
//! valued location (one with a company, internal or transit) and not for consigned stock (an
//! owner other than the company's own partner). Modified: our share is
//! `quantity x value / quantity` of the cost state of (company, product, lot)
//! (`stock.valuation.state`), and the function sums it per group in the report.
//!
//! The quants are read as the caller (record rules apply). The cost states have no access row for
//! any group, so they are read elevated, only after the caller proved to be a stock
//! manager, only for the companies the report options selected (already limited to the caller's
//! allowed companies) and only for the products the caller's own quant read returned.
use std::collections::BTreeMap;

use erp_addon_reporting::functions::{NativeRequest, NativeValues};
use erp_addon_reporting::options::Options;
use erp_addon_reporting::provider::ReportCx;
use erp_addon_reporting::settings::EXPORT_MAX_ROWS;
use erp_sdk::money::{ExactRatio, GridValue, Increment, RoundingMode};
use erp_sdk::prelude::*;
use erp_sdk::serde_json::json;
use erp_sdk::{BoxFuture, Load, SearchOptions};

const MANAGERS: &str = "stock.group_stock_manager";

erp_addon_reporting::expression_function_declarations!(
    QUANT_VALUE,
    "stock_account.quant_value",
    quant_value,
    quant_domain
);

fn quant_value<'a>(
    cx: &'a mut ReportCx<'_>,
    options: &'a Options,
    request: NativeRequest<'a>,
) -> BoxFuture<'a, Result<NativeValues>> {
    Box::pin(value(cx, options, request))
}

fn id_of(value: &GroupValue) -> Result<Option<i64>> {
    match value {
        GroupValue::Records(_) => Ok(value.record_id().map(|r| r.get())),
        GroupValue::False | GroupValue::Null => Ok(None),
        other => Err(Error::internal(format!(
            "stock_account: a quant group key reads as {other:?}"
        ))),
    }
}

fn number(row: &BTreeMap<String, Value>, key: &str) -> Result<Decimal> {
    match row.get(key) {
        Some(Value::Decimal(d)) => Ok(*d),
        Some(Value::Int(n)) => Ok(Decimal::from(*n)),
        Some(Value::Float(f)) => Decimal::try_from(*f)
            .map_err(|_| Error::internal(format!("stock_account: {key} is not finite"))),
        other => Err(Error::internal(format!(
            "stock_account: {key} reads as {other:?}"
        ))),
    }
}

fn id(row: &BTreeMap<String, Value>, key: &str) -> Result<Option<i64>> {
    match row.get(key) {
        Some(Value::Int(n)) => Ok(Some(*n)),
        Some(Value::False) => Ok(None),
        other => Err(Error::internal(format!(
            "stock_account: {key} reads as {other:?}"
        ))),
    }
}

fn quant_domain<'a>(
    env: &'a Env,
    tx: &'a mut Transaction,
    options: &'a Options,
    model: &'a str,
) -> BoxFuture<'a, Result<Domain>> {
    Box::pin(async move { Ok(eligible(env, tx, options, model).await?.0) })
}

async fn eligible(
    env: &Env,
    tx: &mut Transaction,
    options: &Options,
    model: &str,
) -> Result<(Domain, Option<i64>)> {
    if !tx.has_group(env, MANAGERS).await? {
        return Err(Error::access(
            "The value of the stock is for stock managers.",
        ));
    }
    if model != "stock.quant" {
        return Err(Error::validation(
            "Stock valuation requires the stock.quant model.",
        ));
    }
    let companies = &options.companies;
    let rows = env
        .records("res.company")?
        .browse(companies.iter().copied())?
        .read_with(tx, &["partner_id", "currency_id"], Load::Raw)
        .await?;
    let mut company_domains = Vec::with_capacity(rows.len());
    let mut currency = None;
    for row in &rows {
        let company = id(row, "id")?.ok_or_else(|| Error::internal("Company id is missing."))?;
        let partner = id(row, "partner_id")?;
        let expected = id(row, "currency_id")?.ok_or_else(|| {
            Error::validation("Set the company's currency before opening Stock Value.")
        })?;
        if currency
            .replace(expected)
            .is_some_and(|previous| previous != expected)
        {
            return Err(Error::validation(
                "Choose companies with the same currency to show Stock Value.",
            ));
        }
        let mut owned = Domain::from_json(&json!([["owner_id", "=", false]]))?;
        if let Some(partner) = partner {
            owned = owned | Domain::from_json(&json!([["owner_id", "=", partner]]))?;
        }
        company_domains.push(
            owned
                & Domain::from_json(&json!([
                    ["company_id", "=", company],
                    ["location_id.company_id", "=", company]
                ]))?,
        );
    }
    let valued = Domain::or(company_domains)
        & Domain::from_json(&json!([[
            "location_id.usage",
            "in",
            ["internal", "transit"]
        ]]))?;
    Ok((valued, currency))
}

async fn value(
    cx: &mut ReportCx<'_>,
    options: &Options,
    request: NativeRequest<'_>,
) -> Result<NativeValues> {
    let env = cx.env().clone();
    let max_rows = cx.bounds().export_max_rows;
    let limit = u64::try_from(max_rows + 1)
        .map_err(|_| Error::internal("The stock valuation row limit does not fit u64."))?;
    let companies = &options.companies;
    let (valued, currency) = eligible(&env, cx.tx(), options, request.model).await?;
    let mut by: Vec<&str> = request.group.into_iter().collect();
    for field in ["product_id", "lot_id", "company_id"] {
        if !by.contains(&field) {
            by.push(field);
        }
    }
    let args = ReadGroup::new(request.domain & valued)
        .groupby(by.iter().map(|f| (*f).to_owned()))
        .aggregates(["quantity:sum"])
        .limit(limit);
    let groups = env
        .records(request.model)?
        ._read_group(cx.tx(), args)
        .await?;
    if groups.len() > max_rows {
        return Err(Error::validation(format!(
            "The value of the stock needs more than {max_rows} product, lot and company groups (setting {EXPORT_MAX_ROWS})."
        )));
    }
    let at = |name: &str| by.iter().position(|f| *f == name);
    let (product_at, lot_at, company_at) = (
        at("product_id").ok_or_else(|| Error::internal("stock_account: no product key"))?,
        at("lot_id").ok_or_else(|| Error::internal("stock_account: no lot key"))?,
        at("company_id").ok_or_else(|| Error::internal("stock_account: no company key"))?,
    );
    let mut quants = Vec::new();
    for row in groups.rows() {
        let key = |i: usize| -> Result<Option<i64>> {
            id_of(
                row.get(i)
                    .ok_or_else(|| Error::internal("stock_account: a group row lacks its key"))?,
            )
        };
        let quantity = match row.get(by.len()) {
            Some(GroupValue::Decimal(d)) => *d,
            Some(GroupValue::Int(n)) => Decimal::from(*n),
            Some(GroupValue::Float(f)) => Decimal::try_from(*f)
                .map_err(|_| Error::validation("A stock quantity is not a finite number."))?,
            Some(GroupValue::Null | GroupValue::False) => Decimal::ZERO,
            None => {
                return Err(Error::internal(
                    "Stock valuation aggregate row lacks its quantity.",
                ));
            }
            Some(other) => {
                return Err(Error::internal(format!(
                    "stock_account: a quantity sum reads as {other:?}"
                )));
            }
        };
        let group = request.group.map(|_| key(0)).transpose()?.flatten();
        quants.push((
            group,
            key(company_at)?,
            key(product_at)?,
            key(lot_at)?,
            quantity,
        ));
    }
    let mut products: Vec<i64> = quants.iter().filter_map(|q| q.2).collect();
    products.sort_unstable();
    products.dedup();
    if products.is_empty() {
        return Ok(if request.group.is_none() {
            vec![(None, Decimal::ZERO)]
        } else {
            Vec::new()
        });
    }

    let mut lots: Vec<i64> = quants.iter().filter_map(|q| q.3).collect();
    lots.sort_unstable();
    lots.dedup();
    let domain = Domain::from_json(&json!([
        ["company_id", "in", companies],
        ["product_id", "in", products],
        "|",
        ["lot_id", "=", false],
        ["lot_id", "in", lots]
    ]))?;
    // Valuation storage has no user grants. Authorize the manager and the caller's quants first.
    let states = env
        .sudo(true)
        .records("stock.valuation.state")?
        .search_with(cx.tx(), &domain, &SearchOptions::new().limit(limit))
        .await?;
    if states.len() > max_rows {
        return Err(Error::validation(format!(
            "Stock Value exceeds {max_rows} cost states (setting {EXPORT_MAX_ROWS})."
        )));
    }
    // The generic ORM Decimal boundary is narrower than the persisted G256 grid. Fetch
    // exactly the authorized, bounded IDs through the SDK's wide NUMERIC codec. All
    // state operands come from one statement in this transaction, so a concurrent
    // publication cannot mix one generation's grid with another's quantity/currency.
    let state_ids = states.ids();
    let states = cx.tx().query(
        "SELECT company_id,product_id,lot_id,grid_value,quantity,currency_id FROM stk_cost_state WHERE id=ANY($1::bigint[])",
        &[&state_ids],
    ).await?;
    let mut state = BTreeMap::new();
    for row in &states {
        let decode = |error| {
            Error::internal("Stock Value could not decode a published cost state.")
                .with_source(error)
        };
        let company: i64 = row.try_get(0).map_err(decode)?;
        let product: i64 = row.try_get(1).map_err(decode)?;
        let lot: Option<i64> = row.try_get(2).map_err(decode)?;
        let grid: GridValue = row.try_get(3).map_err(decode)?;
        let quantity: Decimal = row.try_get(4).map_err(decode)?;
        let currency: i64 = row.try_get(5).map_err(decode)?;
        state.insert(
            (Some(company), Some(product), lot),
            (grid, quantity, currency),
        );
    }
    let id_currency =
        currency.ok_or_else(|| Error::validation("Select a company currency for Stock Value."))?;
    let currencies = env
        .records("res.currency")?
        .browse([id_currency])?
        .read_with(cx.tx(), &["rounding"], Load::Raw)
        .await?;
    let selected = currencies
        .first()
        .ok_or_else(|| Error::validation("Stock Value could not read the company currency."))?;
    let increment = Increment::new(number(selected, "rounding")?)?;
    let mut sums: BTreeMap<Option<i64>, ExactRatio> = BTreeMap::new();
    for (group, company, product, lot, quantity) in quants {
        if quantity.is_zero() {
            continue;
        }
        // The null-lot cost key prices tracked lots when the product is valued as a whole.
        let published = state.get(&(company, product, lot))
            .or_else(|| state.get(&(company, product, None)))
            .ok_or_else(|| {
                let prod_hint = product
                    .map(|id| format!(" (product id {id})"))
                    .unwrap_or_default();
                Error::validation(format!(
                    "A stock line{prod_hint} has no published valuation. Publish its cost state before requesting Stock Value."
                ))
            })?;
        if Some(published.2) != currency {
            return Err(Error::validation(
                "A stock cost state uses a different currency from its company.",
            ));
        }
        let grid = &published.0;
        let on_hand = published.1;
        if on_hand.is_zero() {
            return Err(Error::validation(
                "A stock line has quantity but its published cost state has zero units.",
            ));
        }
        // Multiply the exact share, then round once per displayed group.
        let fraction = ExactRatio::quotient(quantity, on_hand)?;
        let worth = fraction.checked_mul(&grid.to_ratio()?)?;
        let sum = sums.entry(group).or_insert_with(ExactRatio::zero);
        *sum = sum.checked_add(&worth)?;
    }
    if request.group.is_none() && sums.is_empty() {
        return Ok(vec![(None, Decimal::ZERO)]);
    }
    sums.into_iter()
        .map(|(group, worth)| {
            Ok((
                group,
                worth.quantize(&increment, RoundingMode::HalfAwayFromZero)?,
            ))
        })
        .collect()
}
