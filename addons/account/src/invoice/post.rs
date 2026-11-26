// SPDX-License-Identifier: MIT OR Apache-2.0
//! The posting service (`action_post`) calls [`check_postable`] and
//! [`before_post`]; both do nothing for entries that are not invoices.

use std::collections::BTreeSet;

use erp_sdk::prelude::*;

use super::{is_invoice, is_sale};
use crate::moves::id_cell;

/// The cell of row `i` of a column read for a set of entries; a column shorter than the set is a
/// broken read, never an empty value.
fn nth<'a>(cells: &'a [Cell], i: usize, column: &str) -> Result<&'a Cell> {
    cells
        .get(i)
        .ok_or_else(|| Error::invariant(format!("the {column} column is short")))
}

fn text(cell: &Cell, field: &str) -> Result<Option<Arc<str>>> {
    match cell {
        Cell::Null | Cell::Bool(false) => Ok(None),
        Cell::Str(text) => Ok(Some(text.clone())),
        other => Err(Error::invariant(format!(
            "{field} holds {other:?} instead of a text"
        ))),
    }
}

/// True when the cell holds a date, false when it is empty.
fn dated(cell: &Cell, field: &str) -> Result<bool> {
    match cell {
        Cell::Null | Cell::Bool(false) => Ok(false),
        Cell::Date(_) => Ok(true),
        other => Err(Error::invariant(format!(
            "{field} holds {other:?} instead of a date"
        ))),
    }
}

/// A stored total; an invoice whose totals were never computed has none, which counts as zero.
fn decimal(cell: &Cell) -> Result<Decimal> {
    match cell {
        Cell::Null | Cell::Bool(false) => Ok(Decimal::ZERO),
        Cell::Decimal(value) => Ok(*value),
        other => Err(Error::invariant(format!(
            "an invoice total holds {other:?} instead of an amount"
        ))),
    }
}

/// Refuses the invoices among `moves` that cannot be confirmed, with one message per problem in
/// one error: no customer or vendor, no product line (partial:
/// invoices only), a negative total and a vendor document without its bill date.
/// Entries that are not invoices pass.
///
/// # Errors
///
/// A user error listing every refused invoice and why.
pub async fn check_postable(tx: &mut Transaction, env: &Env, moves: &[i64]) -> Result<()> {
    if moves.is_empty() {
        return Ok(());
    }
    let records = env.records("account.move")?.browse(moves.iter().copied())?;
    let types = records.cells(tx, "move_type").await?;
    let partners = records.cells(tx, "partner_id").await?;
    let totals = records.cells(tx, "amount_total").await?;
    let dates = records.cells(tx, "invoice_date").await?;
    let ids = records.ids();
    // the move type of each entry that is an invoice, by position
    let mut kinds: Vec<Option<Arc<str>>> = Vec::with_capacity(ids.len());
    for i in 0..ids.len() {
        kinds.push(text(nth(&types, i, "move_type")?, "move_type")?.filter(|t| is_invoice(t)));
    }
    let invoices: Vec<i64> = ids
        .iter()
        .zip(&kinds)
        .filter(|(_, kind)| kind.is_some())
        .map(|(id, _)| *id)
        .collect();
    if invoices.is_empty() {
        return Ok(());
    }
    let with_lines: BTreeSet<i64> = {
        let lines = env
            .records("account.move.line")?
            .search(
                tx,
                &Domain::and([
                    Domain::condition("move_id", Operator::In, invoices.clone())?,
                    Domain::condition("display_type", Operator::Eq, "product")?,
                ]),
            )
            .await?;
        let mut owners = BTreeSet::new();
        for cell in lines.cells(tx, "move_id").await? {
            if let Some(id) = id_cell(Some(&cell), "move_id")? {
                owners.insert(id);
            }
        }
        owners
    };
    let mut problems = Vec::new();
    for (i, id) in ids.iter().enumerate() {
        let Some(kind) = kinds.get(i).and_then(Option::as_ref) else {
            continue;
        };
        let sale = is_sale(kind);
        let what = if sale { "Invoice" } else { "Bill" };
        if id_cell(Some(nth(&partners, i, "partner_id")?), "partner_id")?.is_none() {
            let who = if sale { "customer" } else { "vendor" };
            problems.push(format!("{what} {id}: choose the {who} before confirming."));
        }
        if !with_lines.contains(id) {
            problems.push(format!(
                "{what} {id}: add at least one product line before confirming."
            ));
        }
        let total = decimal(nth(&totals, i, "amount_total")?)?;
        if total < Decimal::ZERO {
            problems.push(format!(
                "{what} {id}: the total is negative; record a credit note instead."
            ));
        } else if !sale && total.is_zero() && with_lines.contains(id) {
            problems.push(format!(
                "{what} {id}: the total is zero; enter a non-zero quantity and price before confirming."
            ));
        }
        if !sale && !dated(nth(&dates, i, "invoice_date")?, "invoice_date")? {
            problems.push(format!(
                "{what} {id}: enter the bill date before confirming."
            ));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(Error::user(problems.join("\n")))
    }
}

/// Gives the customer documents among `moves` that have no invoice date today's date in the
/// user's timezone (partial: the rate refresh comes with the currency slice);
/// the write runs the invoice line sync, so the accounting and due dates follow. Vendor documents
/// keep the date their vendor printed: [`check_postable`] asks for it.
///
/// # Errors
///
/// The errors of the write.
pub async fn before_post(tx: &mut Transaction, env: &Env, moves: &[i64]) -> Result<()> {
    if moves.is_empty() {
        return Ok(());
    }
    let records = env.records("account.move")?.browse(moves.iter().copied())?;
    let types = records.cells(tx, "move_type").await?;
    let dates = records.cells(tx, "invoice_date").await?;
    let mut undated = Vec::new();
    for (i, id) in records.ids().into_iter().enumerate() {
        let sale = text(nth(&types, i, "move_type")?, "move_type")?.is_some_and(|k| is_sale(&k));
        if sale && !dated(nth(&dates, i, "invoice_date")?, "invoice_date")? {
            undated.push(id);
        }
    }
    if undated.is_empty() {
        return Ok(());
    }
    let undated = records.browse(undated)?;
    let companies = undated.cells(tx, "company_id").await?;
    let mut groups = std::collections::BTreeMap::<i64, Vec<i64>>::new();
    for (id, company) in undated.ids().into_iter().zip(companies) {
        let company = crate::moves::id_cell(Some(&company), "company_id")?.ok_or_else(|| {
            Error::validation("An invoice needs a company before its date can be set.")
        })?;
        groups.entry(company).or_default().push(id);
    }
    let days = erp_addon_base::company_calendar::business_days(
        tx,
        env,
        &groups.keys().copied().collect::<Vec<_>>(),
    )
    .await?;
    for (company, ids) in groups {
        let day = days
            .get(&company)
            .ok_or_else(|| Error::missing(format!("Company {company} is unavailable.")))?;
        records
            .browse(ids)?
            .write(
                tx,
                RawVals::new().set_cell("invoice_date", Cell::Date(day.date)),
            )
            .await?;
    }
    Ok(())
}
