// SPDX-License-Identifier: MIT OR Apache-2.0
//! The operations of a sales order and the seams of its confirm and cancel chains (B2,
//! "States and every legal transition", D4). Every operation acts on the whole record set: it
//! takes the row locks in id order, reads the states inside the transaction, refuses the whole
//! call when one order is not in a legal source state, and writes `state`, `locked` and the
//! confirmation time through the base write, as the caller.

use std::collections::BTreeSet;

use erp_sdk::Operation;
use erp_sdk::prelude::*;

use crate::order::{Header, SaleOrder, headers};

/// The posted customer invoices and credit notes of the orders: order id, order number, invoice
/// number, invoice id, the first per order first.
const POSTED_INVOICES: &str = "SELECT DISTINCT o.id::int8, o.name, m.name, m.id::int8 FROM sale_invoice_item k \
    JOIN sale_order_line s ON s.id = k.sale_line_id \
    JOIN sale_order o ON o.id = s.order_id \
    JOIN account_move_line l ON l.id = k.invoice_item_id \
    JOIN account_move m ON m.id = l.move_id \
    WHERE s.order_id = ANY($1) AND m.state = 'posted' \
    AND m.move_type IN ('out_invoice', 'out_refund') ORDER BY 1, 3";

/// The draft customer invoices and credit notes of the orders.
const DRAFT_INVOICES: &str = "SELECT DISTINCT l.move_id::int8 FROM sale_invoice_item k \
    JOIN sale_order_line s ON s.id = k.sale_line_id \
    JOIN account_move_line l ON l.id = k.invoice_item_id \
    JOIN account_move m ON m.id = l.move_id \
    WHERE s.order_id = ANY($1) AND m.state = 'draft' \
    AND m.move_type IN ('out_invoice', 'out_refund') ORDER BY 1";

fn decode(error: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::db("Reading the invoices of a sales order failed.").with_source(error)
}

/// The stored ids of `records`; a new record has no invoice yet.
fn stored(records: &Records) -> Vec<i64> {
    records
        .rec_ids()
        .iter()
        .filter_map(|id| id.as_real().map(|id| id.get()))
        .collect()
}

/// The group that may lock and unlock orders (V6).
pub const LOCK_GROUP: &str = "sales_team.group_sale_manager";

/// Lock the orders and read their headers (rule 1 of every operation).
async fn locked_headers(tx: &mut Transaction, records: &Records) -> Result<Vec<Header>> {
    records.lock_for_update(tx, false).await?;
    headers(tx, records).await
}

/// The orders of `orders` that `keep` refuses, described with their state.
fn named(orders: &[Header], refuse: impl Fn(&Header) -> bool) -> Option<String> {
    let refused: Vec<String> = orders
        .iter()
        .filter(|order| refuse(order))
        .map(Header::describe)
        .collect();
    (!refused.is_empty()).then(|| refused.join(", "))
}

fn browse(records: &Records, orders: &[Header], keep: impl Fn(&Header) -> bool) -> Records {
    records.browse_ids(
        orders
            .iter()
            .filter(|order| keep(order))
            .map(|order| order.id),
    )
}

fn state(value: &str) -> RawVals {
    RawVals::new().set_cell("state", Cell::Str(value.into()))
}

fn lock_value(locked: bool) -> RawVals {
    RawVals::new().set_cell("locked", Cell::Bool(locked))
}

/// Superuser mode passes, as the other group-gated operations do (`account` posting).
async fn check_lock_group(tx: &mut Transaction, records: &Records) -> Result<()> {
    let env = records.env();
    if env.su() || tx.has_group(env, LOCK_GROUP).await? {
        Ok(())
    } else {
        Err(Error::access(
            "Only a Sales Administrator can lock or unlock sales orders.",
        ))
    }
}

#[methods(SaleOrder, addon = "sale")]
impl SaleOrder {
    /// Draft an ordinary quotation for a readable customer using the selected company's defaults.
    #[method]
    #[rpc(effect = write, model, assistant = offer)]
    pub async fn action_draft_quotation(
        this: &RecordSet<SaleOrder>,
        tx: &mut Transaction,
        partner_id: i64,
    ) -> Result<Value> {
        let env = this.env().without_default_overrides();
        let company = env
            .company_id()?
            .ok_or_else(|| Error::validation("Select a sales company."))?;
        let partner = env.records("res.partner")?.browse([partner_id])?;
        partner.check_access(tx, Operation::Read).await?;
        if partner.exists(tx).await?.is_empty() {
            return Err(Error::validation("Choose an existing customer."));
        }
        let order = env
            .records("sale.order")?
            .create(
                tx,
                vec![
                    RawVals::new()
                        .set_cell("partner_id", Cell::Id(partner_id))
                        .set_cell("company_id", Cell::Id(company)),
                ],
            )
            .await?;
        Ok(Value::Json(Arc::new(
            erp_sdk::serde_json::json!({"type":"ir.actions.act_window","name":"Quotation",
            "res_model":"sale.order","res_id":order.ensure_one()?.int()?,"views":[[false,"form"]],"target":"current"}),
        )))
    }

    /// Add a catalog product in its native sales unit. The sales model computes the
    /// price, taxes and description for this customer and its selected pricelist.
    #[method]
    #[rpc(effect = write, assistant = offer)]
    pub async fn action_add_quotation_product(
        this: &RecordSet<SaleOrder>,
        tx: &mut Transaction,
        product_id: i64,
        quantity: String,
    ) -> Result<Value> {
        let qty = quantity
            .parse::<Decimal>()
            .map_err(|_| Error::validation("Enter a valid decimal quantity."))?;
        if qty <= Decimal::ZERO {
            return Err(Error::validation(
                "The quotation quantity must be positive.",
            ));
        }
        let order = this.ensure_one()?.as_records();
        order.check_access(tx, Operation::Write).await?;
        order.lock_for_update(tx, true).await?;
        if order.cell(tx, "state").await? != Cell::Str("draft".into()) {
            return Err(Error::validation("Add products to a draft quotation."));
        }
        let env = this.env().without_default_overrides();
        let product = env.records("product.product")?.browse([product_id])?;
        product.check_access(tx, Operation::Read).await?;
        if product.cell(tx, "active").await? != Cell::Bool(true) {
            return Err(Error::validation("Choose a product available for sale."));
        }
        let line = env
            .records("sale.order.line")?
            .create(
                tx,
                vec![
                    RawVals::new()
                        .set_cell("order_id", Cell::Id(order.int()?))
                        .set_cell("product_id", Cell::Id(product_id))
                        .set_cell("product_uom_qty", Cell::Decimal(qty)),
                ],
            )
            .await?;
        Ok(Value::Json(Arc::new(
            erp_sdk::serde_json::json!({"type":"ir.actions.act_window","name":"Quotation Line",
            "res_model":"sale.order.line","res_id":line.ensure_one()?.int()?,"views":[[false,"form"]],"target":"current"}),
        )))
    }

    /// Marks draft quotations as sent.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_quotation_sent(
        this: &RecordSet<SaleOrder>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let records = this.as_records();
        let orders = locked_headers(tx, records).await?;
        if let Some(refused) = named(&orders, |order| order.state != "draft") {
            return Err(Error::validation(format!(
                "A quotation is marked as sent only while it is a draft: {refused}."
            )));
        }
        records.write_base(tx, state("sent")).await
    }

    /// Confirms quotations: the refusal check, the confirmation values, `_action_confirm`, then
    /// the lock of the orders `_should_be_locked` returns ("The confirm chain";
    /// ORM caller can handle a late refusal without committing partial confirmation.
    #[method]
    #[rpc(effect = write, assistant = offer)]
    pub async fn action_confirm(this: &RecordSet<SaleOrder>, tx: &mut Transaction) -> Result<()> {
        let records = this.as_records();
        tx.savepoint(async move |tx| {
            locked_headers(tx, records).await?;
            let refusal: Option<String> =
                records.call(tx, "_confirmation_error_message", ()).await?;
            if let Some(refusal) = refusal {
                return Err(Error::validation(refusal));
            }
            let values: Vals<SaleOrder> =
                records.call(tx, "_prepare_confirmation_values", ()).await?;
            records.write_base(tx, values.into_raw()).await?;
            let () = records.call(tx, "_action_confirm", ()).await?;
            let to_lock: Records = records.call(tx, "_should_be_locked", ()).await?;
            if !to_lock.is_empty() {
                to_lock.write_base(tx, lock_value(true)).await?;
            }
            if crate::settings::automatic_invoice(tx).await? {
                for single in records.iter() {
                    if single.cell(tx, "invoice_status").await? == Cell::Str("to invoice".into()) {
                        let _: Value = single.call(tx, "action_create_draft_invoice", ()).await?;
                    }
                }
            }
            Ok(())
        })
        .await
    }

    /// Cancels the orders not yet cancelled through `_action_cancel`; a locked order refuses the
    /// whole call. Already cancelled orders are left as they are.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_cancel(this: &RecordSet<SaleOrder>, tx: &mut Transaction) -> Result<()> {
        let records = this.as_records();
        let orders = locked_headers(tx, records).await?;
        if let Some(refused) = named(&orders, |order| order.locked) {
            return Err(Error::user(format!(
                "A locked sales order cannot be cancelled; unlock it first: {refused}."
            )));
        }
        let open = browse(records, &orders, |order| order.state != "cancel");
        if open.is_empty() {
            return Ok(());
        }
        open.call(tx, "_action_cancel", ()).await
    }

    /// Sets sent quotations and cancelled orders back to draft; any other state refuses the whole
    /// call.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_draft(this: &RecordSet<SaleOrder>, tx: &mut Transaction) -> Result<()> {
        let records = this.as_records();
        let orders = locked_headers(tx, records).await?;
        if let Some(refused) = named(&orders, |order| {
            !matches!(order.state.as_str(), "sent" | "cancel")
        }) {
            return Err(Error::validation(format!(
                "Only a sent quotation or a cancelled order can be set back to draft: {refused}."
            )));
        }
        records.write_base(tx, state("draft")).await
    }

    /// Locks confirmed orders; Sales administrators only.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_lock(this: &RecordSet<SaleOrder>, tx: &mut Transaction) -> Result<()> {
        let records = this.as_records();
        check_lock_group(tx, records).await?;
        let orders = locked_headers(tx, records).await?;
        if let Some(refused) = named(&orders, |order| order.state != "sale") {
            return Err(Error::validation(format!(
                "Only a confirmed sales order can be locked: {refused}."
            )));
        }
        let open = browse(records, &orders, |order| !order.locked);
        if open.is_empty() {
            return Ok(());
        }
        open.write_base(tx, lock_value(true)).await
    }

    /// Unlocks locked orders; Sales administrators only.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_unlock(this: &RecordSet<SaleOrder>, tx: &mut Transaction) -> Result<()> {
        let records = this.as_records();
        check_lock_group(tx, records).await?;
        let orders = locked_headers(tx, records).await?;
        let locked = browse(records, &orders, |order| order.locked);
        if locked.is_empty() {
            return Ok(());
        }
        locked.write_base(tx, lock_value(false)).await
    }

    /// The first refusal of a confirmation, or `None`: in the base, an order that is neither a
    /// draft nor a sent quotation.
    #[method]
    async fn _confirmation_error_message(
        this: &RecordSet<SaleOrder>,
        tx: &mut Transaction,
    ) -> Result<Option<String>> {
        let orders = headers(tx, this.as_records()).await?;
        Ok(named(&orders, |order| {
            !matches!(order.state.as_str(), "draft" | "sent")
        })
        .map(|refused| format!("Only a draft or sent quotation can be confirmed: {refused}.")))
    }

    /// The values a confirmation writes: the state and the confirmation time.
    /// An override adds its own through field handles and keeps
    /// these (R7).
    #[method]
    async fn _prepare_confirmation_values(
        _this: &RecordSet<SaleOrder>,
        _tx: &mut Transaction,
    ) -> Result<Vals<SaleOrder>> {
        Ok(Vals::new()
            .set(SaleOrder::STATE, Some(Arc::<str>::from("sale")))
            .set(SaleOrder::DATE_ORDER, Some(DateTime::now())))
    }

    /// The step of a confirmation where satellites act; empty in the base.
    #[method]
    async fn _action_confirm(_this: &RecordSet<SaleOrder>, _tx: &mut Transaction) -> Result<()> {
        Ok(())
    }

    /// The orders a confirmation locks: all of them when "Lock confirmed sales" is on, none when
    /// it is off (B4).
    #[method]
    async fn _should_be_locked(
        this: &RecordSet<SaleOrder>,
        tx: &mut Transaction,
    ) -> Result<Records> {
        if crate::settings::lock_confirmed(tx).await? {
            Ok(this.as_records().clone())
        } else {
            Ok(this.as_records().browse([])?)
        }
    }

    /// The cancel step: in the base, an order with a posted invoice or credit note is refused,
    /// otherwise its draft invoices and credit notes are cancelled and the state becomes `cancel`.
    /// Stricter than the reference, which cancels the order and leaves the posted invoice: what
    /// was invoiced stays recorded on the order. The write right on the orders comes first: the
    /// refusal describes their invoices, and names one only when the caller may read it.
    #[method]
    async fn _action_cancel(this: &RecordSet<SaleOrder>, tx: &mut Transaction) -> Result<()> {
        let records = this.as_records();
        records.check_access(tx, Operation::Write).await?;
        tx.flush().await?;
        if let Some(row) = tx
            .query(POSTED_INVOICES, &[&stored(records)])
            .await?
            .first()
        {
            let order: String = row.try_get(1).map_err(decode)?;
            let number: Option<String> = row.try_get(2).map_err(decode)?;
            let invoice: i64 = row.try_get(3).map_err(decode)?;
            let readable = !records
                .env()
                .records("account.move")?
                .browse([invoice])?
                .filtered_access(tx, Operation::Read)
                .await?
                .is_empty();
            let named = match number {
                Some(number) if readable => format!(" {number}"),
                _ => String::new(),
            };
            return Err(Error::user(format!(
                "The sales order {order} cannot be cancelled: its invoice{named} is posted, and a posted invoice stays on its order. Correct the invoice with a credit note instead."
            )));
        }
        let drafts: Vec<i64> = tx
            .query(DRAFT_INVOICES, &[&stored(records)])
            .await?
            .iter()
            .map(|row| row.try_get(0).map_err(decode))
            .collect::<Result<_>>()?;
        erp_addon_account::moves::cancel_document_drafts(tx, records.env(), &drafts).await?;
        records.write_base(tx, state("cancel")).await
    }

    /// The orders whose Cancel would be refused: in the base, those with a posted invoice or
    /// credit note. A bridge that refuses the cancel step adds its own through an override that
    /// calls `sup`.
    #[method]
    async fn _cancel_blocked(this: &RecordSet<SaleOrder>, tx: &mut Transaction) -> Result<Records> {
        let records = this.as_records();
        tx.flush().await?;
        let mut blocked = BTreeSet::new();
        for row in tx.query(POSTED_INVOICES, &[&stored(records)]).await? {
            blocked.insert(row.try_get::<_, i64>(0).map_err(decode)?);
        }
        records.browse(blocked)
    }

    #[compute(depends = [])]
    async fn _compute_cancel_blocked(
        this: &RecordSet<SaleOrder>,
        tx: &mut Transaction,
        out: &mut Assign<'_, SaleOrder>,
    ) -> Result<()> {
        let records = this.as_records();
        // an onchange snapshot is a new record whose origin is the stored order
        let row = |id: &RecId| id.as_real().or(id.origin()).map(|id| id.get());
        let stored = records.browse(records.rec_ids().iter().filter_map(row))?;
        let blocked: Records = stored.call(tx, "_cancel_blocked", ()).await?;
        let blocked: BTreeSet<i64> = blocked.ids().into_iter().collect();
        for (index, id) in records.rec_ids().iter().enumerate() {
            let held = row(id).is_some_and(|id| blocked.contains(&id));
            out.set(index, SaleOrder::CANCEL_BLOCKED, held)?;
        }
        Ok(())
    }
}
