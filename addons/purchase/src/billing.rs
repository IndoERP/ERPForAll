// SPDX-License-Identifier: MIT OR Apache-2.0
//! What an order has billed and still has to bill: the bill control of each product (ordered or
//! received quantities), the billed quantity and the quantity to bill of each line, and the
//! billing status of the order. Accounting owns every bill; these fields read only the quantity,
//! type and state of the bill items linked to a line, so a buyer sees them without any access to
//! the bills.

use std::collections::{BTreeMap, BTreeSet};

use erp_addon_product::ProductTemplate;
use erp_sdk::Operation;
use erp_sdk::prelude::*;

use crate::line::PurchaseOrderLine;
use crate::order::PurchaseOrder;

/// Bill control "On ordered quantities".
pub const ORDERED: &str = "purchase";
/// Bill control "On received quantities".
pub const RECEIVED: &str = "receive";

/// `product.template` with its bill control.
#[extend(ProductTemplate, addon = "purchase")]
pub struct PurchaseBillControl {
    /// Bill the ordered or the received quantities of this product. A service bills ordered
    /// quantities; any other product takes the Purchase setting "Bill control" when it is created
    /// or its type changes, and keeps a value chosen afterwards.
    #[field(
        selection,
        selection = [("purchase", "On ordered quantities"), ("receive", "On received quantities")],
        compute = "_compute_purchase_method",
        store = true,
        readonly = false,
        string = "Bill Control"
    )]
    pub purchase_method: Option<Arc<str>>,
}

fn decode(error: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::db("The billed purchase quantities could not be read.").with_source(error)
}

fn real_ids(ids: &[RecId]) -> Vec<i64> {
    ids.iter()
        .filter_map(|id| id.as_real().or(id.origin()).map(|id| id.get()))
        .collect()
}

#[methods(ProductTemplate, addon = "purchase")]
impl PurchaseBillControl {
    /// A write that names `type` with the value a product already has is no type change: the
    /// current types are loaded first, and the write passes over a value equal to the loaded one,
    /// so a bill control chosen by hand stays.
    #[method(override)]
    async fn write(
        this: &RecordSet<ProductTemplate>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        if vals.contains("type") {
            this.as_records().check_access(tx, Operation::Write).await?;
            this.mapped(tx, ProductTemplate::TYPE).await?;
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }

    /// A service bills ordered quantities; any other product takes the setting's. This runs on
    /// every change of the type, not only when a product leaves service (goods that become a
    /// combo take the setting's control again too), because the reference does the same; a write
    /// that names the current type is no change ([`Self::write`]). It also runs when the column
    /// is filled at install or upgrade; a value chosen by hand stays until the next type change.
    #[compute(depends = ["type"])]
    async fn _compute_purchase_method(
        this: &RecordSet<ProductTemplate>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ProductTemplate>,
    ) -> Result<()> {
        let kinds = this.mapped(tx, ProductTemplate::TYPE).await?;
        let mut default: Option<Arc<str>> = None;
        for (index, kind) in kinds.into_iter().enumerate() {
            let policy: Arc<str> = if kind.as_deref() == Some("service") {
                ORDERED.into()
            } else if let Some(policy) = &default {
                policy.clone()
            } else {
                let policy: Arc<str> = crate::settings::default_bill_control(tx).await?.into();
                default = Some(policy.clone());
                policy
            };
            out.set(index, PurchaseBillControl::PURCHASE_METHOD, Some(policy))?;
        }
        Ok(())
    }
}

/// Write the pending values the SQL below reads (an item quantity edited in this transaction, a
/// new link), without flushing unrelated compute graphs.
async fn flush_bill_inputs(tx: &mut Transaction, env: &Env) -> Result<()> {
    for (model, names) in [
        (
            "purchase.direct.bill.line",
            &["purchase_line_id", "order_id", "bill_id", "bill_line_id"][..],
        ),
        // the type with the account and amounts its CHECKs tie to it, never one of them alone
        (
            "account.move.line",
            &[
                "quantity",
                "display_type",
                "account_id",
                "balance",
                "amount_currency",
                "move_id",
            ][..],
        ),
        ("account.move", &["state", "move_type"][..]),
    ] {
        let records = env.records(model)?;
        let fields = names
            .iter()
            .map(|name| {
                records
                    .spec()
                    .field(name)
                    .map(|field| field.id)
                    .ok_or_else(|| {
                        Error::programming(format!("The billed quantity reads {model}.{name}."))
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        tx.flush_fields(records.model_id(), &fields).await?;
    }
    Ok(())
}

/// The signed quantity of the draft and posted vendor bill items linked to each of `lines`:
/// bills add, vendor credit notes subtract, cancelled documents and removed items count nothing.
/// A linked item keeps the line's product and unit (the bill integrity checks), so the sum is in
/// the line's unit.
async fn billed_quantities(
    tx: &mut Transaction,
    env: &Env,
    lines: &[i64],
) -> Result<BTreeMap<i64, Decimal>> {
    let mut billed = BTreeMap::new();
    if lines.is_empty() {
        return Ok(billed);
    }
    flush_bill_inputs(tx, env).await?;
    let rows = tx
        .query(
            "SELECT s.purchase_line_id, SUM(CASE m.move_type WHEN 'in_invoice' THEN i.quantity
                 ELSE -i.quantity END)::text
             FROM purchase_direct_bill_line s
             JOIN account_move_line i ON i.id = s.bill_line_id AND i.move_id = s.bill_id
             JOIN account_move m ON m.id = s.bill_id
             WHERE s.purchase_line_id = ANY($1) AND i.display_type = 'product'
               AND m.move_type IN ('in_invoice', 'in_refund') AND m.state IN ('draft', 'posted')
             GROUP BY s.purchase_line_id",
            &[&lines],
        )
        .await?;
    for row in rows {
        let line: i64 = row.try_get(0).map_err(decode)?;
        let quantity: String = row.try_get(1).map_err(decode)?;
        let quantity = quantity.parse().map_err(|_| {
            Error::validation("The billed purchase quantity exceeds the decimal range.")
        })?;
        billed.insert(line, quantity);
    }
    Ok(billed)
}

/// The bill control of each product of `products`: services always bill ordered quantities;
/// any other product follows its `purchase_method` (defaulting to received quantities when unset).
async fn product_policies(
    tx: &mut Transaction,
    env: &Env,
    products: impl IntoIterator<Item = RecId>,
) -> Result<BTreeMap<RecId, bool>> {
    let products = env.records("product.product")?.browse_ids(products);
    let ids = products.rec_ids().to_vec();
    let kinds = products.values(tx, "type").await?;
    let values = products.values(tx, "purchase_method").await?;
    Ok(ids
        .into_iter()
        .zip(kinds.into_iter().zip(values))
        .map(|(id, (kind, policy))| {
            let is_service = matches!(kind, Value::Str(k) if k.as_ref() == "service");
            let ordered = is_service || matches!(policy, Value::Str(p) if p.as_ref() == ORDERED);
            (id, ordered)
        })
        .collect())
}

#[methods(PurchaseOrderLine, addon = "purchase")]
impl PurchaseOrderLine {
    /// Recomputed when a linked bill item is created, changes quantity, is removed, or its bill
    /// is posted, cancelled, reset or deleted.
    #[compute(depends = [
        "direct_bill_line_ids",
        "direct_bill_line_ids.bill_line_id",
        "direct_bill_line_ids.bill_line_id.quantity",
        "direct_bill_line_ids.bill_line_id.display_type",
        "direct_bill_line_ids.bill_id.state",
        "direct_bill_line_ids.bill_id.move_type"
    ])]
    async fn _compute_qty_invoiced(
        this: &RecordSet<PurchaseOrderLine>,
        tx: &mut Transaction,
        out: &mut Assign<'_, PurchaseOrderLine>,
    ) -> Result<()> {
        let rec_ids = this.as_records().rec_ids().to_vec();
        let env = this.env().clone();
        let billed = billed_quantities(tx, &env, &real_ids(&rec_ids)).await?;
        for (index, id) in rec_ids.iter().enumerate() {
            let quantity = id
                .as_real()
                .or(id.origin())
                .and_then(|id| billed.get(&id.get()))
                .copied()
                .unwrap_or_default();
            out.set(index, PurchaseOrderLine::QTY_INVOICED, quantity)?;
        }
        Ok(())
    }

    /// On a confirmed order: the ordered quantity (bill control on ordered quantities) or the
    /// received one, less the billed quantity; negative when bills exceed it. Zero otherwise.
    #[compute(depends = [
        "order_id.state",
        "product_id",
        "product_id.purchase_method",
        "product_qty",
        "qty_received",
        "qty_invoiced"
    ])]
    async fn _compute_qty_to_invoice(
        this: &RecordSet<PurchaseOrderLine>,
        tx: &mut Transaction,
        out: &mut Assign<'_, PurchaseOrderLine>,
    ) -> Result<()> {
        let states = this.mapped(tx, PurchaseOrderLine::STATE).await?;
        let products = this.mapped(tx, PurchaseOrderLine::PRODUCT_ID).await?;
        let ordered = this.mapped(tx, PurchaseOrderLine::PRODUCT_QTY).await?;
        let received = this.mapped(tx, PurchaseOrderLine::QTY_RECEIVED).await?;
        let billed = this.mapped(tx, PurchaseOrderLine::QTY_INVOICED).await?;
        let env = this.env().clone();
        let by_ordered = product_policies(tx, &env, products.iter().flatten().copied()).await?;
        let rows = states
            .into_iter()
            .zip(products)
            .zip(ordered.into_iter().zip(received).zip(billed));
        for (index, ((state, product), ((ordered, received), billed))) in rows.enumerate() {
            let quantity = if state.as_deref() == Some("purchase") {
                let ordered_control =
                    product.and_then(|p| by_ordered.get(&p)).copied() == Some(true);
                let base = if ordered_control { ordered } else { received };
                base.checked_sub(billed).ok_or_else(|| {
                    Error::validation("The quantity to bill of a purchase line is out of range.")
                })?
            } else {
                Decimal::ZERO
            };
            out.set(index, PurchaseOrderLine::QTY_TO_INVOICE, quantity)?;
        }
        Ok(())
    }
}

#[methods(PurchaseOrder, addon = "purchase")]
impl PurchaseOrder {
    /// `no` outside a confirmed order; `to invoice` while a line has a quantity to bill (also a
    /// negative one: bills above the control); `invoiced` when none has and a bill item is linked
    /// to the order; else `no`.
    #[compute(depends = [
        "state",
        "order_line",
        "order_line.qty_to_invoice",
        "order_line.direct_bill_line_ids",
        "order_line.direct_bill_line_ids.bill_line_id"
    ])]
    async fn _compute_invoice_status(
        this: &RecordSet<PurchaseOrder>,
        tx: &mut Transaction,
        out: &mut Assign<'_, PurchaseOrder>,
    ) -> Result<()> {
        let states = this.mapped(tx, PurchaseOrder::STATE).await?;
        let lines = this.mapped(tx, PurchaseOrder::ORDER_LINE).await?;
        let set = RecordSet::<PurchaseOrderLine>::try_from(
            this.env()
                .records("purchase.order.line")?
                .browse_ids(lines.iter().flatten().copied()),
        )?;
        let open: BTreeSet<RecId> = set
            .as_records()
            .rec_ids()
            .iter()
            .copied()
            .zip(set.mapped(tx, PurchaseOrderLine::QTY_TO_INVOICE).await?)
            .filter_map(|(line, quantity)| (!quantity.is_zero()).then_some(line))
            .collect();
        let orders = real_ids(this.as_records().rec_ids());
        let mut billed = BTreeSet::new();
        if !orders.is_empty() {
            let env = this.env().clone();
            flush_bill_inputs(tx, &env).await?;
            for row in tx
                .query(
                    "SELECT DISTINCT order_id FROM purchase_direct_bill_line
                     WHERE order_id = ANY($1) AND bill_line_id IS NOT NULL",
                    &[&orders],
                )
                .await?
            {
                billed.insert(row.try_get::<_, i64>(0).map_err(decode)?);
            }
        }
        let rec_ids = this.as_records().rec_ids().to_vec();
        for (index, ((id, state), lines)) in rec_ids.iter().zip(states).zip(lines).enumerate() {
            let status = if state.as_deref() != Some("purchase") {
                "no"
            } else if lines.iter().any(|line| open.contains(line)) {
                "to invoice"
            } else if id
                .as_real()
                .or(id.origin())
                .is_some_and(|id| billed.contains(&id.get()))
            {
                "invoiced"
            } else {
                "no"
            };
            out.set(
                index,
                PurchaseOrder::INVOICE_STATUS,
                Some(Arc::<str>::from(status)),
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]
    use super::*;

    #[test]
    fn the_policy_values_are_the_declared_choices() {
        let declared: Vec<&str> = crate::settings::BILL_CONTROL_CHOICES
            .iter()
            .map(|(value, _)| *value)
            .collect();
        assert_eq!(declared, [ORDERED, RECEIVED]);
    }
}
