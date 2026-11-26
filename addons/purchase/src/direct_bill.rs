// SPDX-License-Identifier: MIT OR Apache-2.0
//! Vendor bills for the received goods of one order, created in Accounting as the caller.

use erp_sdk::prelude::*;
use erp_sdk::serde_json::json;
use erp_sdk::{Command, Context, FieldOp, Operation};
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::order::PurchaseOrder;

pub(super) const SOURCE_LIMIT: &str = "purchase.max_direct_bill_source_rows";

fn parameters() -> Result<Vec<erp_sdk::settings::ParameterDecl>> {
    Ok(vec![erp_sdk::settings::ParameterDecl::integer(
        SOURCE_LIMIT,
        "Source rows in one direct vendor bill",
        10_000,
        (1, 100_000),
    )?])
}
erp_sdk::system_parameters!(parameters);

#[model(
    name = "purchase.direct.bill.line",
    description = "Direct Vendor Bill Source",
    rec_name = "purchase_line_id",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    constraints = [
        ("_source", "UNIQUE (purchase_line_id, bill_id)", "A purchase line has one linked item per vendor bill."),
        ("_item", "UNIQUE (bill_line_id)", "A direct bill item has one purchase source.")
    ],
)]
struct DirectBillLine {
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        ondelete = "restrict"
    )]
    company_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "purchase.order",
        required = true,
        index = true,
        ondelete = "restrict"
    )]
    order_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "purchase.order.line",
        required = true,
        ondelete = "restrict"
    )]
    purchase_line_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "account.move",
        required = true,
        index = true,
        ondelete = "cascade"
    )]
    bill_id: Option<RecId>,
    // Removing a draft item keeps the document association and preserves deliberate draft edits.
    #[field(many2one, comodel = "account.move.line", ondelete = "set null")]
    bill_line_id: Option<RecId>,
}

#[methods(PurchaseOrder, addon = "purchase")]
impl PurchaseOrder {
    /// Create the first bill for the quantity to bill of each line (ordered or received, by the
    /// product's bill control), or reopen its accounting document.
    ///
    /// # Errors
    ///
    /// Refuses multiple orders, an unauthorized order or item, nothing to bill or missing journal.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_create_vendor_bill(
        this: &RecordSet<PurchaseOrder>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let order = this.as_records().ensure_one()?;
        tx.savepoint(async |tx| draft_or_open(tx, order, BillRequest::First).await)
            .await
    }

    /// Bill the quantity still to bill of each line: ordered or received, by the product's bill
    /// control, less the quantities on active direct bills.
    ///
    /// # Errors
    ///
    /// Refuses unapproved orders, unreadable source records and bills above the bill control.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_bill_remaining(
        this: &RecordSet<PurchaseOrder>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let order = this.as_records().ensure_one()?;
        tx.savepoint(async |tx| draft_or_open(tx, order, BillRequest::Remaining).await)
            .await
    }
}

pub(super) fn decode(error: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::db("The order's vendor bill sources could not be read.").with_source(error)
}

async fn document_action(tx: &mut Transaction, id: i64) -> Result<Value> {
    let rows = tx.query("SELECT res_id FROM ir_model_data WHERE module='account' AND name='view_invoice_form' AND model='ir.ui.view'", &[]).await?;
    let [view] = rows.as_slice() else {
        return Err(Error::invariant(
            "The native accounting invoice form is missing.",
        ));
    };
    let form: i64 = view.try_get(0).map_err(decode)?;
    Ok(Value::Json(Arc::new(json!({
        "type": "ir.actions.act_window", "name": "Vendor Bill",
        "res_model": "account.move", "res_id": id, "views": [[form,"form"]], "target": "new"
    }))))
}

pub(super) async fn authorize_fields(
    tx: &mut Transaction,
    records: &Records,
    fields: &[&str],
) -> Result<()> {
    records.check_access(tx, Operation::Read).await?;
    for field in fields {
        if !tx
            .has_field_access(records.env(), records.spec().name, field, FieldOp::Read)
            .await?
        {
            return Err(Error::access(
                "Creating a vendor bill requires access to its purchase source fields.",
            ));
        }
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BillRequest {
    First,
    Remaining,
}

async fn draft_or_open(
    tx: &mut Transaction,
    order: &Records,
    request: BillRequest,
) -> Result<Value> {
    order.check_access(tx, Operation::Write).await?;
    authorize_fields(
        tx,
        order,
        &[
            "state",
            "name",
            "company_id",
            "partner_id",
            "currency_id",
            "order_line",
        ],
    )
    .await?;
    tx.flush().await?;
    order.lock_for_update(tx, true).await?;
    let order_id = order.ids()[0];
    let Cell::Id(company) = order.cell(tx, "company_id").await? else {
        return Err(Error::invariant("The purchase order has no company."));
    };
    let mut context = Context::new();
    context.set("allowed_company_ids", Value::list([Value::Int(company)]));
    let env = order.env().with_context_replaced(context);
    let bills = env.records("account.move")?;
    bills.check_access(tx, Operation::Read).await?;
    // Only this operation inserts the links; the parent row lock serializes every writer. A bill
    // the order's cancel cancelled is history: a confirmed-again order gets a new first bill.
    if request == BillRequest::First && let Some(row) = tx.query("SELECT k.bill_id FROM purchase_direct_bill_line k JOIN account_move m ON m.id = k.bill_id WHERE k.company_id=$1 AND k.order_id=$2 AND m.state <> 'cancel' ORDER BY k.bill_id LIMIT 1", &[&company, &order_id]).await?.first() {
        let bill = row.try_get(0).map_err(decode)?;
        bills.browse([bill])?.check_access(tx, Operation::Read).await?;
        return document_action(tx, bill).await;
    }
    bills.check_access(tx, Operation::Create).await?;
    env.records("account.move.line")?
        .check_access(tx, Operation::Create)
        .await?;
    if order.cell(tx, "state").await? != Cell::Str("purchase".into()) {
        return Err(Error::user(
            "Confirm and approve the purchase order before creating a vendor bill.",
        ));
    }
    let limit = erp_sdk::params::get_int(tx, &crate::ADDON, SOURCE_LIMIT).await?;
    let history = match request {
        BillRequest::First => crate::bill_history::History::default(),
        BillRequest::Remaining => {
            crate::bill_history::read(tx, &env, company, order_id, limit).await?
        }
    };
    let (items, sources) = billable_items(tx, &env, company, order_id, limit).await?;
    if items.is_empty() {
        if let Some(bill) = history.latest_active {
            return document_action(tx, bill).await;
        }
        return Err(Error::user(
            "Nothing on this purchase order is ready to bill. Receive goods first, or set the \
             product's bill control to ordered quantities.",
        ));
    }
    if history.link_count > 0 {
        let new_total = history
            .link_count
            .checked_add(sources.len())
            .and_then(|n| i64::try_from(n).ok());
        if new_total.is_none_or(|n| n > limit) {
            return Err(Error::validation(format!(
                "Adding this bill would exceed {SOURCE_LIMIT} ({limit}) linked purchase items."
            )));
        }
    }
    let header = RawVals::new()
        .set_cell("move_type", Cell::Str("in_invoice".into()))
        .set_cell("company_id", Cell::Id(company))
        .set_cell("partner_id", order.cell(tx, "partner_id").await?)
        .set_cell("currency_id", order.cell(tx, "currency_id").await?)
        .set_cell("ref", order.cell(tx, "name").await?)
        .set_commands("invoice_line_ids", items);
    // A row lock alone leaves the PO visible to a transaction whose snapshot predates
    // this bill. Publish a new tuple version with the document, preserving audit fields.
    let changed = tx
        .execute(
            "UPDATE purchase_order SET write_date = write_date WHERE id = $1 AND company_id = $2",
            &[&order_id, &company],
        )
        .await?;
    if changed != 1 {
        return Err(Error::invariant(
            "The vendor bill lost its locked purchase order.",
        ));
    }
    let created = bills.create(tx, vec![header]).await?;
    let bill = created.ensure_one()?.ids()[0];
    if matches!(created.cell(tx, "amount_total").await?, Cell::Decimal(total) if total < Decimal::ZERO)
    {
        return Err(Error::user(
            "A negative bill total requires a vendor credit note (a later release).",
        ));
    }
    link_items(tx, company, order_id, bill, &sources).await?;
    created.check_access(tx, Operation::Read).await?;
    document_action(tx, bill).await
}

/// One bill item per line with a positive quantity to bill, the quantity being the line's
/// `qty_to_invoice`: the button and the order's billing status read the same number.
async fn billable_items(
    tx: &mut Transaction,
    env: &erp_sdk::Env,
    company: i64,
    order_id: i64,
    limit: i64,
) -> Result<(Vec<Command>, Vec<i64>)> {
    let probe = limit
        .checked_add(1)
        .ok_or_else(|| Error::invariant("Invalid direct bill source limit."))?;
    let bound = usize::try_from(limit)
        .map_err(|_| Error::invariant("Invalid direct bill source limit."))?;
    let rows = tx
        .query(
            "SELECT id FROM purchase_order_line WHERE order_id=$1 ORDER BY sequence,id LIMIT $2",
            &[&order_id, &probe],
        )
        .await?;
    let ids: Vec<i64> = rows
        .iter()
        .map(|row| row.try_get(0))
        .collect::<std::result::Result<_, _>>()
        .map_err(decode)?;
    let lines = env
        .records("purchase.order.line")?
        .browse(ids.iter().copied())?;
    authorize_fields(
        tx,
        &lines,
        &[
            "product_id",
            "qty_to_invoice",
            "uom_id",
            "name",
            "price_unit",
            "discount",
            "tax_ids",
            "analytic_account_id",
            "asset_id",
        ],
    )
    .await?;
    let too_large = || {
        Error::validation(format!(
            "The direct bill exceeds {SOURCE_LIMIT} ({limit}) purchase lines or tax links."
        ))
    };
    if ids.len() > bound {
        return Err(too_large());
    }
    // Positive-quantity candidates can depend on stock completion, so lock the bounded identity
    // set before reading its quantitative fields. Tax membership has its own bounded witness.
    lines.lock_for_update(tx, true).await?;
    let tax_rows=tx.query("SELECT purchase_order_line_id,account_tax_id FROM account_tax_purchase_order_line_rel
        WHERE purchase_order_line_id=ANY($1) ORDER BY purchase_order_line_id,account_tax_id LIMIT $2",&[&ids,&probe]).await?;
    let mut taxes_by_line = BTreeMap::<i64, Vec<RecId>>::new();
    let mut tax_ids = std::collections::BTreeSet::new();
    for row in &tax_rows {
        let source: i64 = row.try_get(0).map_err(decode)?;
        let tax: i64 = row.try_get(1).map_err(decode)?;
        let id = RecId::from_real(tax)
            .ok_or_else(|| Error::invariant("A purchase tax has no saved identity."))?;
        tax_ids.insert(tax);
        taxes_by_line.entry(source).or_default().push(id);
    }
    if !tax_ids.is_empty() {
        env.records("account.tax")?
            .browse(tax_ids)?
            .check_access(tx, Operation::Read)
            .await?;
    }
    if tax_rows.len() > bound {
        return Err(too_large());
    }
    let mappings = [
        ("product_id", "product_id"),
        ("uom_id", "product_uom_id"),
        ("name", "name"),
        ("price_unit", "price_unit"),
        ("discount", "discount"),
        ("analytic_account_id", "analytic_account_id"),
        ("asset_id", "asset_id"),
    ];
    let mut columns = Vec::with_capacity(mappings.len());
    for (source, target) in mappings {
        columns.push((target, lines.cells(tx, source).await?));
    }
    let asset_cells = columns
        .iter()
        .find(|(target, _)| *target == "asset_id")
        .map(|(_, col)| col.as_slice())
        .unwrap_or(&[]);
    let mut cip_account_by_asset = BTreeMap::<i64, i64>::new();
    let mut asset_ids_vec: Vec<i64> = asset_cells
        .iter()
        .filter_map(|c| match c {
            Cell::Id(id) => Some(*id),
            _ => None,
        })
        .collect();
    asset_ids_vec.sort_unstable();
    asset_ids_vec.dedup();
    if !asset_ids_vec.is_empty() {
        env.records("account.asset")?
            .browse(asset_ids_vec.clone())?
            .check_access(tx, Operation::Read)
            .await?;
        let arows = tx
            .query(
                "SELECT id::int8, account_asset_id::int8, name::text \
                 FROM account_asset \
                 WHERE id = ANY($1::int8[]) AND company_id = $2 \
                   AND COALESCE(asset_type, 'normal') = 'cip'",
                &[&asset_ids_vec, &company],
            )
            .await?;
        for ar in arows {
            let aid: i64 = ar.try_get(0).map_err(decode)?;
            let acc_id: Option<i64> = ar.try_get(1).map_err(decode)?;
            let aname: String = ar.try_get(2).map_err(decode)?;
            let acc_id = acc_id.ok_or_else(|| {
                Error::validation(format!(
                    "CIP Asset '{aname}' has no Fixed Asset / CIP GL account configured."
                ))
            })?;
            cip_account_by_asset.insert(aid, acc_id);
        }
    }
    let quantities = lines.cells(tx, "qty_to_invoice").await?;
    let mut items = Vec::new();
    let mut sources = Vec::new();
    for (index, id) in ids.into_iter().enumerate() {
        let Some(Cell::Decimal(remaining)) = quantities.get(index).cloned() else {
            return Err(Error::invariant(
                "The quantity to bill of a purchase line is not a number.",
            ));
        };
        if remaining < Decimal::ZERO {
            return Err(Error::user(
                "One or more vendor bills exceed what their products' bill control allows (the \
                 ordered or the received quantity), or goods were returned; a vendor credit note is \
                 a later release. Review those items in Accounting before billing more goods.",
            ));
        }
        if remaining.is_zero() {
            continue;
        }
        let taxes = taxes_by_line.remove(&id).unwrap_or_default();
        let position = i32::try_from(sources.len())
            .map_err(|_| Error::validation("Too many purchase lines to number the bill."))?;
        let mut item = RawVals::new()
            .set_cell("sequence", Cell::Int(position))
            .set_cell("quantity", Cell::Decimal(remaining))
            .set_commands("tax_ids", [Command::Set(taxes)]);
        for (target, column) in &columns {
            let value = column.get(index).ok_or_else(|| {
                Error::invariant("A purchase line disappeared while preparing its vendor bill.")
            })?;
            item = item.set_cell(*target, value.clone());
        }
        if let Some(Cell::Id(aid)) = asset_cells.get(index)
            && let Some(&cip_acc) = cip_account_by_asset.get(aid)
        {
            item = item.set_cell("account_id", Cell::Id(cip_acc));
        }
        items.push(Command::Create(item));
        sources.push(id);
    }
    Ok((items, sources))
}

async fn link_items(
    tx: &mut Transaction,
    company: i64,
    order_id: i64,
    bill: i64,
    sources: &[i64],
) -> Result<()> {
    tx.flush().await?;
    // Match assigned commercial positions, never product names, prices or PostgreSQL row order.
    let probe = i64::try_from(sources.len())
        .ok()
        .and_then(|count| count.checked_add(1))
        .ok_or_else(|| Error::invariant("Invalid direct bill item count."))?;
    let item_rows = tx.query("SELECT sequence::int8,id FROM account_move_line WHERE move_id=$1 AND display_type='product' ORDER BY sequence LIMIT $2", &[&bill,&probe]).await?;
    if item_rows.len() != sources.len() {
        return Err(Error::invariant(
            "The draft vendor bill did not preserve its purchase items.",
        ));
    }
    let mut bill_items = Vec::with_capacity(sources.len());
    for (expected, row) in item_rows.iter().enumerate() {
        let position: i64 = row.try_get(0).map_err(decode)?;
        if usize::try_from(position).ok() != Some(expected) {
            return Err(Error::invariant(
                "The draft vendor bill did not preserve its purchase item positions.",
            ));
        }
        bill_items.push(row.try_get::<_, i64>(1).map_err(decode)?);
    }
    // The private association only: the bill and its items were created as the caller. Through the
    // ORM, so the billed quantities of the source lines and the order's status follow.
    let links = sources
        .iter()
        .zip(bill_items)
        .map(|(source, item)| {
            RawVals::new()
                .set_cell("company_id", Cell::Id(company))
                .set_cell("order_id", Cell::Id(order_id))
                .set_cell("bill_id", Cell::Id(bill))
                .set_cell("purchase_line_id", Cell::Id(*source))
                .set_cell("bill_line_id", Cell::Id(item))
        })
        .collect();
    tx.env()
        .sudo(true)
        .records("purchase.direct.bill.line")?
        .create(tx, links)
        .await?;
    tx.flush().await?;
    let linked = tx.env().records("account.move")?.browse([bill])?;
    tx.invalidate_records(&linked, Some(&["purchase_order_count"]))
        .await?;
    crate::line::invalidate_bill_comparisons(tx).await
}
