// SPDX-License-Identifier: MIT OR Apache-2.0
//! One commercial source per customer invoice item. Accounting owns every document state.

use std::collections::{BTreeMap, BTreeSet};

use erp_addon_account::moves::{AccountMove, AccountMoveLine};
use erp_sdk::prelude::*;
use erp_sdk::{FieldOp, Operation};

use crate::{SaleOrder, SaleOrderLine};

#[model(name="sale.invoice.item", description="Sales Invoice Line Link", rec_name="sale_line_id",
    company_access = parent(edge = "sale_line_id"),
    constraints=[("_invoice_item", "UNIQUE (invoice_item_id)", "An invoice item has only one sales order line.")])]
struct InvoiceItemLink {
    #[field(
        many2one,
        comodel = "sale.order.line",
        required = true,
        ondelete = "restrict",
        index = true
    )]
    sale_line_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "account.move.line",
        required = true,
        ondelete = "restrict"
    )]
    invoice_item_id: Option<RecId>,
}

#[extend(AccountMoveLine, addon = "sale")]
struct SalesInvoiceItem {
    /// Sales owns the association. Accounting stores no additional columns.
    #[field(
        many2one,
        comodel = "sale.order.line",
        compute = "_compute_sale_line",
        inverse = "_inverse_sale_line",
        compute_sudo = false,
        copy = false,
        groups = "sales_team.group_sale_salesman",
        string = "Sales Order Line"
    )]
    sale_line_id: Option<RecId>,
}

#[extend(AccountMove, addon = "sale")]
struct SalesInvoiceHeader {}

const INCOMPATIBLE: &str = "A sales invoice line must match the confirmed order's billing partner, company, currency, product and unit on a customer invoice or credit note.";

fn decode(err: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::db("Reading the sales invoice association failed.").with_source(err)
}

async fn authorize_sources(tx: &mut Transaction, records: &Records) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    records.check_access(tx, Operation::Write).await?;
    readable_fields(
        tx,
        records,
        &["order_id", "product_id", "product_uom_id", "display_type"],
    )
    .await?;
    let orders = records.related(tx, "order_id").await?;
    orders.check_access(tx, Operation::Read).await?;
    readable_fields(
        tx,
        &orders,
        &["state", "partner_invoice_id", "company_id", "currency_id"],
    )
    .await?;
    Ok(())
}

pub(super) async fn readable_fields(
    tx: &mut Transaction,
    records: &Records,
    fields: &[&str],
) -> Result<()> {
    for field in fields {
        if !tx
            .has_field_access(records.env(), records.spec().name, field, FieldOp::Read)
            .await?
        {
            return Err(Error::access(
                "Reading the invoice source requires access to its matching fields.",
            ));
        }
    }
    Ok(())
}

#[methods(AccountMoveLine, addon = "sale")]
impl SalesInvoiceItem {
    #[compute(depends=["move_id"], depends_context=["uid", "allowed_company_ids"])]
    async fn _compute_sale_line(
        this: &RecordSet<AccountMoveLine>,
        tx: &mut Transaction,
        out: &mut Assign<'_, AccountMoveLine>,
    ) -> Result<()> {
        let mut sources = BTreeMap::new();
        let rows = tx.query(
            "SELECT invoice_item_id::int8, sale_line_id::int8 FROM sale_invoice_item WHERE invoice_item_id=ANY($1)",
            &[&this.ids()],
        ).await?;
        for row in rows {
            let invoice: i64 = row.try_get(0).map_err(decode)?;
            let sale: i64 = row.try_get(1).map_err(decode)?;
            sources.insert(invoice, sale);
        }
        let orders = this
            .env()
            .records("sale.order.line")?
            .browse(sources.values().copied())?;
        if !orders.is_empty() {
            orders.check_access(tx, Operation::Read).await?;
        }
        for (index, id) in this.rec_ids().iter().enumerate() {
            let source = id
                .as_real()
                .or(id.origin())
                .and_then(|v| sources.get(&v.get()))
                .and_then(|v| RecId::from_real(*v));
            out.set(index, SalesInvoiceItem::SALE_LINE_ID, source)?;
        }
        Ok(())
    }

    #[method]
    async fn _inverse_sale_line(
        this: &RecordSet<AccountMoveLine>,
        tx: &mut Transaction,
    ) -> Result<()> {
        this.as_records().check_access(tx, Operation::Write).await?;
        let desired = this.mapped(tx, SalesInvoiceItem::SALE_LINE_ID).await?;
        tx.savepoint(async |tx| assign_sources(tx, this.as_records(), desired).await)
            .await
    }

    #[method(override)]
    async fn create(
        this: &RecordSet<AccountMoveLine>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        if this.env().context().contains("default_sale_line_id") {
            return Err(Error::validation(
                "Choose the sales source on the invoice item itself.",
            ));
        }
        if !vals_list.iter().any(|vals| vals.contains("sale_line_id")) {
            return sup.call(tx, this.as_records(), (vals_list,)).await;
        }
        if !tx
            .has_field_access(
                this.env(),
                "account.move.line",
                "sale_line_id",
                FieldOp::Write,
            )
            .await?
        {
            return Err(Error::access(
                "The sales source of an invoice item is restricted.",
            ));
        }
        // Non-stored inverse values can already have been evicted during a nested invoice create.
        // Capture the caller's explicit source just as Accounting does for company account codes.
        let desired = vals_list
            .iter_mut()
            .map(|vals| match vals.remove("sale_line_id") {
                None | Some(Cell::Null) => Ok(None),
                Some(Cell::Id(id)) if id > 0 => Ok(RecId::from_real(id)),
                _ => Err(Error::validation(
                    "Choose an existing sales order line as the invoice source.",
                )),
            })
            .collect::<Result<Vec<_>>>()?;
        tx.savepoint(async |tx| {
            let items: Records = sup.call(tx, this.as_records(), (vals_list,)).await?;
            assign_sources(tx, &items, desired).await?;
            tx.invalidate_records(&items, Some(&["sale_line_id"]))
                .await?;
            Ok(items)
        })
        .await
    }

    #[method(override)]
    async fn unlink(
        this: &RecordSet<AccountMoveLine>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        this.as_records()
            .check_access(tx, Operation::Unlink)
            .await?;
        tx.savepoint(async |tx| {
            remove_links(tx, this.as_records(), false).await?;
            sup.call(tx, this.as_records(), ()).await
        })
        .await
    }

    #[constrains(
        "sale_line_id",
        "product_id",
        "product_uom_id",
        "display_type",
        "move_id"
    )]
    async fn _check_sales_item(
        this: &RecordSet<AccountMoveLine>,
        tx: &mut Transaction,
    ) -> Result<()> {
        check_links(tx, this.ids(), "l.id").await
    }
}

#[methods(AccountMove, addon = "sale")]
impl SalesInvoiceHeader {
    #[method(override)]
    async fn unlink(this: &RecordSet<AccountMove>, tx: &mut Transaction, sup: Super) -> Result<()> {
        this.check_access(tx, Operation::Unlink).await?;
        tx.savepoint(async |tx| {
            remove_links(tx, this.as_records(), true).await?;
            sup.call(tx, this.as_records(), ()).await
        })
        .await
    }

    /// Posting refuses an invoice or credit note whose sales order is not confirmed (cancelled or
    /// back in quotation): what it invoices would no longer match an order. The orders are
    /// share-locked, so a cancel of one of them at the same time waits for the posting or fails.
    /// Nothing about the order is named: the poster may not read it.
    #[method(override)]
    async fn action_post(
        this: &RecordSet<AccountMove>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        let moves = this.as_records();
        if !moves.is_empty() {
            moves.check_access(tx, Operation::Write).await?;
            tx.flush().await?;
            let rows = tx
                .query(
                    "SELECT o.state::text FROM sale_order o WHERE o.id IN ( \
                         SELECT s.order_id FROM sale_invoice_item k \
                         JOIN sale_order_line s ON s.id = k.sale_line_id \
                         JOIN account_move_line l ON l.id = k.invoice_item_id \
                         WHERE l.move_id = ANY($1)) ORDER BY o.id FOR SHARE",
                    &[&moves.ids()],
                )
                .await?;
            for row in rows {
                let state: Option<String> = row.try_get(0).map_err(decode)?;
                if state.as_deref() != Some("sale") {
                    return Err(Error::user(
                        "This invoice comes from a sales order that is cancelled or back in quotation, so it cannot be posted. Confirm the order again, or remove its lines from the invoice.",
                    ));
                }
            }
        }
        sup.call(tx, moves, ()).await
    }

    /// Propagate sales order line links onto reversed customer credit notes / invoices so
    /// `qty_invoiced`, `qty_to_invoice`, and `invoice_status` remain accurate.
    #[method(override)]
    async fn _after_reverse_move(
        this: &RecordSet<AccountMove>,
        tx: &mut Transaction,
        source_move_id: i64,
        line_pairs: Value,
        sup: Super,
    ) -> Result<()> {
        sup.call(tx, this.as_records(), (source_move_id, line_pairs.clone()))
            .await?;
        let mut pairs: Vec<(i64, i64)> = Vec::new();
        if let Value::List(entries) = &line_pairs {
            for entry in entries.iter() {
                if let Value::List(pair) = entry
                    && pair.len() == 2
                    && let (Value::Int(src), Value::Int(rev)) = (&pair[0], &pair[1])
                {
                    pairs.push((*src, *rev));
                }
            }
        }
        if pairs.is_empty() {
            return Ok(());
        }
        let src_ids: Vec<i64> = pairs.iter().map(|(s, _)| *s).collect();
        let rows = tx
            .query(
                "SELECT k.invoice_item_id::int8, k.sale_line_id::int8, s.order_id::int8 \
                 FROM sale_invoice_item k \
                 JOIN sale_order_line s ON s.id = k.sale_line_id \
                 WHERE k.invoice_item_id = ANY($1::int8[])",
                &[&src_ids],
            )
            .await?;
        if rows.is_empty() {
            return Ok(());
        }
        let rev_by_src: BTreeMap<i64, i64> = pairs.into_iter().collect();
        let mut new_links = Vec::new();
        let mut rev_line_ids = Vec::new();
        let mut sale_line_ids = BTreeSet::new();
        let mut order_ids = BTreeSet::new();
        for row in rows {
            let src_line_id: i64 = row.try_get(0).map_err(decode)?;
            let sale_line_id: i64 = row.try_get(1).map_err(decode)?;
            let order_id: i64 = row.try_get(2).map_err(decode)?;
            if let Some(&rev_line_id) = rev_by_src.get(&src_line_id) {
                rev_line_ids.push(rev_line_id);
                sale_line_ids.insert(sale_line_id);
                order_ids.insert(order_id);
                new_links.push(
                    RawVals::new()
                        .set_cell("invoice_item_id", Cell::Id(rev_line_id))
                        .set_cell("sale_line_id", Cell::Id(sale_line_id)),
                );
            }
        }
        if !new_links.is_empty() {
            let links = this.env().sudo(true).records("sale.invoice.item")?;
            links.create(tx, new_links).await?;
            tx.flush().await?;
            check_links(tx, rev_line_ids.clone(), "l.id").await?;
            let items = this
                .env()
                .records("account.move.line")?
                .browse(rev_line_ids)?;
            tx.invalidate_records(&items, Some(&["sale_line_id"]))
                .await?;
            let sol = this
                .env()
                .records("sale.order.line")?
                .browse(sale_line_ids)?;
            tx.invalidate_records(
                &sol,
                Some(&["qty_invoiced", "qty_to_invoice", "invoice_status"]),
            )
            .await?;
            let orders = this.env().records("sale.order")?.browse(order_ids)?;
            tx.invalidate_records(&orders, Some(&["invoice_status", "invoice_count"]))
                .await?;
        }
        Ok(())
    }

    #[constrains("partner_id", "currency_id", "company_id", "move_type")]
    async fn _check_sales_items(this: &RecordSet<AccountMove>, tx: &mut Transaction) -> Result<()> {
        check_links(tx, this.ids(), "m.id").await
    }
}

async fn remove_links(tx: &mut Transaction, owner: &Records, header: bool) -> Result<()> {
    let sql = if header {
        "SELECT link.id::int8 FROM sale_invoice_item link JOIN account_move_line l ON l.id = link.invoice_item_id WHERE l.move_id=ANY($1)"
    } else {
        "SELECT id::int8 FROM sale_invoice_item WHERE invoice_item_id=ANY($1)"
    };
    let ids = tx
        .query(sql, &[&owner.ids()])
        .await?
        .iter()
        .map(|row| row.try_get::<_, i64>(0).map_err(decode))
        .collect::<Result<Vec<_>>>()?;
    if !ids.is_empty() {
        owner
            .env()
            .sudo(true)
            .records("sale.invoice.item")?
            .browse(ids)?
            .unlink(tx)
            .await?;
    }
    Ok(())
}

#[methods(SaleOrderLine, addon = "sale")]
impl SaleOrderLine {
    #[constrains("order_id", "product_id", "product_uom_id", "display_type")]
    async fn _check_invoice_item_identity(
        this: &RecordSet<SaleOrderLine>,
        tx: &mut Transaction,
    ) -> Result<()> {
        check_links(tx, this.ids(), "s.id").await
    }
}

#[methods(SaleOrder, addon = "sale")]
impl SaleOrder {
    #[constrains("company_id", "partner_invoice_id", "currency_id")]
    async fn _check_invoice_billing_identity(
        this: &RecordSet<SaleOrder>,
        tx: &mut Transaction,
    ) -> Result<()> {
        check_links(tx, this.ids(), "o.id").await
    }
}

async fn assign_sources(
    tx: &mut Transaction,
    items: &Records,
    desired: Vec<Option<RecId>>,
) -> Result<()> {
    let mut affected: BTreeSet<RecId> = desired.iter().flatten().copied().collect();
    let mut previous = BTreeMap::new();
    let previous_links = tx.query(
        "SELECT id::int8, invoice_item_id::int8, sale_line_id::int8 FROM sale_invoice_item WHERE invoice_item_id=ANY($1)",
        &[&items.ids()],
    ).await?;
    for row in previous_links {
        let link: i64 = row.try_get(0).map_err(decode)?;
        let invoice: i64 = row.try_get(1).map_err(decode)?;
        let sale: i64 = row.try_get(2).map_err(decode)?;
        affected.insert(
            RecId::from_real(sale)
                .ok_or_else(|| Error::invariant("Invalid stored invoice source."))?,
        );
        previous.insert(invoice, (link, sale));
    }
    let sales = items.env().records("sale.order.line")?.browse_ids(affected);
    authorize_sources(tx, &sales).await?;
    sales.lock_for_update(tx, false).await?;
    let target_orders = items
        .env()
        .records("sale.order.line")?
        .browse_ids(desired.iter().flatten().copied())
        .related(tx, "order_id")
        .await?;
    if target_orders
        .cells(tx, "state")
        .await?
        .iter()
        .any(|c| !matches!(c, Cell::Str(state) if state.as_ref() == "sale"))
    {
        return Err(Error::validation(
            "Choose a product line from a confirmed sales order.",
        ));
    }
    // Only the validated association is elevated. Invoice and sales operations retain
    // the requesting principal and neither document can be created through this model.
    let links = items.env().sudo(true).records("sale.invoice.item")?;
    for (invoice, target) in items.ids().into_iter().zip(desired) {
        let target = target.and_then(RecId::as_real).map(|r| r.get());
        let old = previous.get(&invoice).copied();
        if old.map(|(_, s)| s) == target {
            continue;
        }
        if let Some((link, _)) = old {
            links.browse([link])?.unlink(tx).await?;
        }
        if let Some(sale) = target {
            links
                .create(
                    tx,
                    vec![
                        RawVals::new()
                            .set_cell("invoice_item_id", Cell::Id(invoice))
                            .set_cell("sale_line_id", Cell::Id(sale)),
                    ],
                )
                .await?;
        }
    }
    check_links(tx, items.ids(), "l.id").await
}

async fn check_links(tx: &mut Transaction, ids: Vec<i64>, owner: &'static str) -> Result<()> {
    // The changed owner is already authorized. Read only an incompatibility bit. Billing never receives the identity of another order.
    let query = format!("SELECT 1 FROM account_move_line l
        JOIN account_move m ON m.id = l.move_id
        JOIN sale_invoice_item link ON link.invoice_item_id=l.id JOIN sale_order_line s ON s.id=link.sale_line_id JOIN sale_order o ON o.id = s.order_id
        WHERE {owner} = ANY($1) AND (m.move_type NOT IN ('out_invoice','out_refund')
        OR m.partner_id IS DISTINCT FROM o.partner_invoice_id
        OR m.company_id IS DISTINCT FROM o.company_id OR m.currency_id IS DISTINCT FROM o.currency_id
        OR s.display_type IS NOT NULL OR l.display_type IS DISTINCT FROM 'product'
        OR (NOT COALESCE(s.is_downpayment, false) AND s.product_id IS NULL)
        OR s.product_id IS DISTINCT FROM l.product_id
        OR s.product_uom_id IS DISTINCT FROM l.product_uom_id) LIMIT 1");
    if !tx.query(&query, &[&ids]).await?.is_empty() {
        return Err(Error::validation(INCOMPATIBLE));
    }
    Ok(())
}

/// Group only the requested sales lines. Sales may see its own invoiced quantity without
/// gaining any access to the journal, invoice header, price or payment data.
pub(crate) async fn quantities(
    this: &RecordSet<SaleOrderLine>,
    tx: &mut Transaction,
) -> Result<BTreeMap<RecId, Decimal>> {
    let ids = this.as_records().ids();
    let rows = tx
        .query(
            "SELECT link.sale_line_id::int8, SUM(CASE m.move_type WHEN 'out_invoice'
        THEN l.quantity ELSE -l.quantity END)::text FROM sale_invoice_item link JOIN account_move_line l ON l.id=link.invoice_item_id
        JOIN account_move m ON m.id=l.move_id
        WHERE link.sale_line_id=ANY($1) AND l.display_type='product'
        AND m.move_type IN ('out_invoice','out_refund') AND m.state IN ('draft','posted')
        GROUP BY link.sale_line_id",
            &[&ids],
        )
        .await?;
    let mut result = BTreeMap::new();
    for row in rows {
        let id: i64 = row.try_get(0).map_err(decode)?;
        let quantity: String = row.try_get(1).map_err(decode)?;
        let id =
            RecId::from_real(id).ok_or_else(|| Error::internal("Invalid persisted sales line."))?;
        let quantity = quantity.parse().map_err(|_| {
            Error::validation("The invoiced sales quantity exceeds the decimal range.")
        })?;
        result.insert(id, quantity);
    }
    Ok(result)
}
