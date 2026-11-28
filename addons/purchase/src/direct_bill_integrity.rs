// SPDX-License-Identifier: MIT OR Apache-2.0
//! Keep existing draft-edit privileges while preserving the source of a direct bill.

use erp_addon_account::moves::{AccountMove, AccountMoveLine};
use erp_sdk::Operation;
use erp_sdk::prelude::*;

#[extend(AccountMove, addon = "purchase")]
struct DirectBill {}

#[methods(AccountMove, addon = "purchase")]
impl DirectBill {
    #[method(override)]
    async fn write(
        this: &RecordSet<AccountMove>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        preserve_source(tx, this.as_records(), vals, sup,
            &["company_id","currency_id","partner_id","move_type","journal_id"],
            "SELECT 1 FROM purchase_direct_bill_line s
             JOIN account_move m ON m.id=s.bill_id JOIN purchase_order p ON p.id=s.order_id
             WHERE m.id=ANY($1) AND (m.move_type NOT IN ('in_invoice', 'in_refund')
             OR m.company_id IS DISTINCT FROM s.company_id OR m.currency_id IS DISTINCT FROM p.currency_id
             OR m.partner_id IS DISTINCT FROM p.partner_id) LIMIT 1").await
    }

    /// Propagate purchase order bill-line links onto reversed vendor credit notes / bills so
    /// `qty_invoiced`, `qty_to_invoice`, `invoice_status`, and AVC commitments stay accurate.
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
        let Some(rev_id) = this.as_records().ids().into_iter().next() else {
            return Ok(());
        };
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
                "SELECT company_id::int8, order_id::int8, purchase_line_id::int8, bill_line_id::int8 \
                 FROM purchase_direct_bill_line \
                 WHERE bill_id = $1 AND bill_line_id = ANY($2::int8[])",
                &[&source_move_id, &src_ids],
            )
            .await?;
        if rows.is_empty() {
            return Ok(());
        }
        let rev_by_src: std::collections::BTreeMap<i64, i64> = pairs.into_iter().collect();
        let mut links = Vec::new();
        let mut order_ids = std::collections::BTreeSet::new();
        for row in rows {
            let company_id: i64 = row.try_get(0).map_err(|e| {
                Error::db("Reading purchase reversal link company failed.").with_source(e)
            })?;
            let order_id: i64 = row.try_get(1).map_err(|e| {
                Error::db("Reading purchase reversal link order failed.").with_source(e)
            })?;
            let purchase_line_id: i64 = row.try_get(2).map_err(|e| {
                Error::db("Reading purchase reversal link line failed.").with_source(e)
            })?;
            let src_line_id: i64 = row.try_get(3).map_err(|e| {
                Error::db("Reading purchase reversal link item failed.").with_source(e)
            })?;
            if let Some(&rev_line_id) = rev_by_src.get(&src_line_id) {
                order_ids.insert(order_id);
                links.push(
                    RawVals::new()
                        .set_cell("company_id", Cell::Id(company_id))
                        .set_cell("order_id", Cell::Id(order_id))
                        .set_cell("bill_id", Cell::Id(rev_id))
                        .set_cell("purchase_line_id", Cell::Id(purchase_line_id))
                        .set_cell("bill_line_id", Cell::Id(rev_line_id)),
                );
            }
        }
        if !links.is_empty() {
            tx.env()
                .sudo(true)
                .records("purchase.direct.bill.line")?
                .create(tx, links)
                .await?;
            tx.flush().await?;
            tx.invalidate_records(this.as_records(), Some(&["purchase_order_count"]))
                .await?;
            let orders = tx.env().records("purchase.order")?.browse(order_ids)?;
            tx.invalidate_records(&orders, Some(&["invoice_status", "invoice_count"]))
                .await?;
            crate::line::invalidate_bill_comparisons(tx).await?;
        }
        Ok(())
    }

    /// Posting refuses a vendor bill whose purchase order is not confirmed (cancelled or back to a
    /// request for quotation): what it bills would no longer match an order. The orders are
    /// share-locked, so a cancel of one of them at the same time waits for the posting or fails.
    /// Nothing about the order is named: the poster may not read it.
    #[method(override)]
    #[rpc(effect = write, assistant = offer)]
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
                    "SELECT p.state::text FROM purchase_order p WHERE p.id IN ( \
                         SELECT s.order_id FROM purchase_direct_bill_line s WHERE s.bill_id = ANY($1)) \
                     ORDER BY p.id FOR SHARE",
                    &[&moves.ids()],
                )
                .await?;
            for row in rows {
                let state: Option<String> = row.try_get(0).map_err(|error| {
                    Error::db("Reading the purchase order of a vendor bill failed.")
                        .with_source(error)
                })?;
                if state.as_deref() != Some("purchase") {
                    return Err(Error::user(
                        "This vendor bill comes from a purchase order that is cancelled or back to a request for quotation, so it cannot be posted. Confirm the order again, or remove its lines from the bill.",
                    ));
                }
            }
        }
        sup.call(tx, moves, ()).await
    }
}

#[extend(AccountMoveLine, addon = "purchase")]
struct DirectBillItem {}

#[methods(AccountMoveLine, addon = "purchase")]
impl DirectBillItem {
    #[method(override)]
    async fn write(
        this: &RecordSet<AccountMoveLine>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        preserve_source(tx, this.as_records(), vals, sup,
            &["move_id","product_id","product_uom_id","display_type"],
            "SELECT 1 FROM purchase_direct_bill_line s
             JOIN account_move_line l ON l.id=s.bill_line_id JOIN purchase_order_line p ON p.id=s.purchase_line_id
             WHERE l.id=ANY($1) AND (l.move_id IS DISTINCT FROM s.bill_id OR l.display_type IS DISTINCT FROM 'product'
             OR l.product_id IS DISTINCT FROM p.product_id OR l.product_uom_id IS DISTINCT FROM p.uom_id) LIMIT 1").await
    }
}

/// Check the final purchase values after product defaults have also set the unit.
pub(crate) async fn check_purchase_items(tx: &mut Transaction, lines: &Records) -> Result<()> {
    tx.flush().await?;
    let changed = tx
        .query(
            "SELECT 1 FROM purchase_direct_bill_line s
         JOIN account_move_line b ON b.id=s.bill_line_id
         JOIN purchase_order_line p ON p.id=s.purchase_line_id
         WHERE p.id=ANY($1) AND (p.product_id IS DISTINCT FROM b.product_id
             OR p.uom_id IS DISTINCT FROM b.product_uom_id) LIMIT 1",
            &[&lines.ids()],
        )
        .await?;
    if !changed.is_empty() {
        return Err(Error::validation(
            "A purchase order item linked to a vendor bill must keep its billed product and unit.",
        ));
    }
    Ok(())
}

async fn preserve_source(
    tx: &mut Transaction,
    records: &Records,
    vals: RawVals,
    sup: Super<(RawVals,), ()>,
    fields: &[&str],
    mismatch: &str,
) -> Result<()> {
    if !fields.iter().any(|field| vals.contains(field)) || records.is_empty() {
        return sup.call(tx, records, (vals,)).await;
    }
    records.check_access(tx, Operation::Write).await?;
    tx.savepoint(async move |tx| {
        let ()=sup.call(tx,records,(vals,)).await?;
        tx.flush().await?;
        if !tx.query(mismatch,&[&records.ids()]).await?.is_empty() {
            return Err(Error::validation("A vendor bill linked to a purchase order must keep its supplier, company, currency, and source item identity."));
        }
        Ok(())
    }).await
}
