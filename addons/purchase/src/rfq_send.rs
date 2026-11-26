// SPDX-License-Identifier: MIT OR Apache-2.0
//! Send RFQ by Email (`purchase.rfq.send`), Receipt Reminders, and Alternative RFQs + Line
//! Comparison (`purchase.alternative.wizard`).

use erp_sdk::prelude::*;
use erp_sdk::serde_json::json;
use erp_sdk::{Operation, ValueMap};

use crate::line::PurchaseOrderLine;
use crate::order::{PurchaseOrder, strip_default_context};

fn db(err: impl std::fmt::Display) -> Error {
    Error::db(format!("Purchase RFQ action query failed: {err}"))
}

/// Pure exact-decimal helper picking the lowest-subtotal alternative quote line ID.
pub fn select_best_quote_line(candidates: &[(i64, Decimal)]) -> Option<i64> {
    candidates
        .iter()
        .min_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)))
        .map(|(id, _)| *id)
}

/// `purchase.rfq.send`: transient email composer for sending RFQs and Purchase Orders to vendors.
#[model(
    name = "purchase.rfq.send",
    description = "Send RFQ by Email",
    kind = "transient",
    transient_max_age_seconds = 3600,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct PurchaseRfqSend {
    /// RFQ / PO being emailed.
    #[field(
        many2one,
        comodel = "purchase.order",
        required = true,
        ondelete = "cascade",
        string = "Purchase Order"
    )]
    pub order_id: Option<RecId>,
    /// Company from the order.
    #[field(
        many2one,
        comodel = "res.company",
        related = "order_id.company_id",
        store = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Vendor recipient.
    #[field(many2one, comodel = "res.partner", string = "Vendor")]
    pub partner_id: Option<RecId>,
    /// Vendor email address.
    #[field(char, string = "Email To")]
    pub email_to: Option<Arc<str>>,
    /// Email subject.
    #[field(char, required = true, string = "Subject")]
    pub subject: Option<Arc<str>>,
    /// Email message body.
    #[field(text, string = "Message")]
    pub body: Option<Arc<str>>,
}

#[methods(PurchaseRfqSend, addon = "purchase")]
impl PurchaseRfqSend {
    #[method(override)]
    async fn default_get(
        this: &RecordSet<PurchaseRfqSend>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<ValueMap> {
        let mut values: ValueMap = sup.call(tx, this.as_records(), (names.clone(),)).await?;
        let oid = match this.env().context().get("default_order_id") {
            Some(Value::Int(id)) => Some(id),
            _ => None,
        };
        if let Some(order_id) = oid {
            let order = this.env().records("purchase.order")?.browse([order_id])?;
            order.check_access(tx, Operation::Read).await?;
            let oname = match order.cell(tx, "name").await? {
                Cell::Str(s) => s.to_string(),
                Cell::Null => format!("PO-{order_id}"),
                other => {
                    return Err(Error::programming(format!(
                        "purchase.order.name returned {other:?}"
                    )));
                }
            };
            let total = match order.cell(tx, "amount_total").await? {
                Cell::Decimal(d) => d.to_string(),
                Cell::Null => "0.00".to_owned(),
                other => {
                    return Err(Error::programming(format!(
                        "purchase.order.amount_total returned {other:?}"
                    )));
                }
            };
            let partner_id = match order.cell(tx, "partner_id").await? {
                Cell::Id(pid) => Some(pid),
                Cell::Null => None,
                other => {
                    return Err(Error::programming(format!(
                        "purchase.order.partner_id returned {other:?}"
                    )));
                }
            };
            if names.iter().any(|n| n == "partner_id")
                && !values.contains_key("partner_id")
                && let Some(pid) = partner_id
            {
                values.insert("partner_id".into(), Value::Int(pid));
            }
            if names.iter().any(|n| n == "email_to")
                && !values.contains_key("email_to")
                && let Some(pid) = partner_id
            {
                match this
                    .env()
                    .records("res.partner")?
                    .browse([pid])?
                    .cell(tx, "email")
                    .await?
                {
                    Cell::Str(email) => {
                        values.insert("email_to".into(), Value::Str(email));
                    }
                    Cell::Null => {}
                    other => {
                        return Err(Error::programming(format!(
                            "res.partner.email returned {other:?}"
                        )));
                    }
                }
            }
            if names.iter().any(|n| n == "subject") && !values.contains_key("subject") {
                values.insert(
                    "subject".into(),
                    Value::Str(Arc::from(format!("Request for Quotation {oname}"))),
                );
            }
            if names.iter().any(|n| n == "body") && !values.contains_key("body") {
                values.insert(
                    "body".into(),
                    Value::Str(Arc::from(format!(
                        "Hello,\n\nPlease find attached our Request for Quotation {oname} (estimated total: {total}). Kindly reply with your best pricing and delivery schedule.\n\nBest regards,"
                    ))),
                );
            }
        }
        Ok(values)
    }

    /// Marks draft RFQs as Sent, logs the RFQ communication, and notifies the caller.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_send_rfq(
        this: &RecordSet<PurchaseRfqSend>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let wizard = this.ensure_one()?;
        wizard
            .as_records()
            .check_access(tx, Operation::Write)
            .await?;
        let order_id = wizard
            .get(tx, PurchaseRfqSend::ORDER_ID)
            .await?
            .ok_or_else(|| Error::validation("Choose a purchase order to send."))?
            .get();
        let email_to = wizard
            .get(tx, PurchaseRfqSend::EMAIL_TO)
            .await?
            .unwrap_or_default();
        if email_to.trim().is_empty() {
            return Err(Error::user("Enter a recipient email before sending."));
        }
        let subject = wizard
            .get(tx, PurchaseRfqSend::SUBJECT)
            .await?
            .unwrap_or_default();
        if subject.trim().is_empty() {
            return Err(Error::user("Enter a subject before sending."));
        }
        let body = wizard
            .get(tx, PurchaseRfqSend::BODY)
            .await?
            .unwrap_or_default();

        let env = strip_default_context(wizard.env());
        let order = env.records("purchase.order")?.browse([order_id])?;
        order.check_access(tx, Operation::Write).await?;

        tx.savepoint(async |tx| {
            tx.flush().await?;
            order.lock_for_update(tx, true).await?;
            let state = order.cell(tx, "state").await?;
            if state == Cell::Str("cancel".into()) {
                return Err(Error::user(
                    "Cancelled RFQs cannot be sent. Set the RFQ back to draft first.",
                ));
            }
            if state == Cell::Str("draft".into()) {
                let _: () = order.call(tx, "_mark_rfqs_as_sent", ()).await?;
            }
            let _: () = order
                .call(
                    tx,
                    "_on_rfq_emailed",
                    (
                        email_to.trim().to_owned(),
                        subject.trim().to_owned(),
                        body.to_string(),
                    ),
                )
                .await?;
            wizard.as_records().unlink(tx).await
        })
        .await?;

        Ok(Value::Json(Arc::new(json!({
            "type": "ir.actions.client",
            "tag": "display_notification",
            "params": {
                "message": format!("RFQ logged for {}.", email_to.trim()),
                "type": "success",
                "next": {"type": "ir.actions.act_window_close"},
            },
        }))))
    }
}

/// `purchase.alternative.wizard`: creates a linked alternative RFQ for another vendor to compare
/// pricing and lead times side by side.
#[model(
    name = "purchase.alternative.wizard",
    description = "Create Alternative RFQ",
    kind = "transient",
    transient_max_age_seconds = 3600,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct PurchaseAlternativeWizard {
    /// Original RFQ being compared.
    #[field(
        many2one,
        comodel = "purchase.order",
        required = true,
        ondelete = "cascade",
        string = "Original RFQ"
    )]
    pub origin_po_id: Option<RecId>,
    /// Company from the original RFQ.
    #[field(
        many2one,
        comodel = "res.company",
        related = "origin_po_id.company_id",
        store = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Alternative vendor to request a quote from.
    #[field(
        many2one,
        comodel = "res.partner",
        required = true,
        check_company = true,
        string = "Alternative Vendor"
    )]
    pub partner_id: Option<RecId>,
    /// Copy product lines (re-priced using the alternative vendor's pricelist).
    #[field(boolean, default = true, string = "Copy Products")]
    pub copy_products: bool,
}

#[methods(PurchaseAlternativeWizard, addon = "purchase")]
impl PurchaseAlternativeWizard {
    #[method]
    #[rpc(effect = write)]
    pub async fn action_create_alternative(
        this: &RecordSet<PurchaseAlternativeWizard>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let wizard = this.ensure_one()?;
        wizard
            .as_records()
            .check_access(tx, Operation::Write)
            .await?;
        let origin_id = wizard
            .get(tx, PurchaseAlternativeWizard::ORIGIN_PO_ID)
            .await?
            .ok_or_else(|| Error::validation("Missing original RFQ."))?
            .get();
        let vendor_id = wizard
            .get(tx, PurchaseAlternativeWizard::PARTNER_ID)
            .await?
            .ok_or_else(|| Error::user("Select an alternative vendor."))?
            .get();
        let copy_products = wizard
            .get(tx, PurchaseAlternativeWizard::COPY_PRODUCTS)
            .await?;

        let clean_env = strip_default_context(wizard.env());
        let origin = clean_env.records("purchase.order")?.browse([origin_id])?;
        origin.check_access(tx, Operation::Write).await?;

        let alt_id = tx
            .savepoint(async |tx| {
                tx.flush().await?;
                origin.lock_for_update(tx, true).await?;
                if origin.cell(tx, "state").await? == Cell::Str("cancel".into()) {
                    return Err(Error::user(
                        "Cannot create an alternative from a cancelled RFQ.",
                    ));
                }
                let Cell::Id(company_id) = origin.cell(tx, "company_id").await? else {
                    return Err(Error::invariant("Original RFQ has no company."));
                };
                let env = clean_env.with_company(company_id)?;

                let group_key = match origin.cell(tx, "rfq_group_key").await? {
                    Cell::Str(s) if !s.trim().is_empty() => s.to_string(),
                    _ => {
                        let key = format!("ALT-{origin_id}");
                        origin
                            .write(
                                tx,
                                RawVals::new()
                                    .set_cell("rfq_group_key", Cell::Str(key.clone().into())),
                            )
                            .await?;
                        key
                    }
                };

                let mut po_vals = RawVals::new();
                po_vals.insert("partner_id".into(), Cell::Id(vendor_id));
                po_vals.insert("company_id".into(), Cell::Id(company_id));
                po_vals.insert("currency_id".into(), origin.cell(tx, "currency_id").await?);
                po_vals.insert("rfq_group_key".into(), Cell::Str(group_key.into()));
                if let Cell::Id(req_id) = origin.cell(tx, "requisition_id").await? {
                    po_vals.insert("requisition_id".into(), Cell::Id(req_id));
                }
                if let Cell::Str(orig_name) = origin.cell(tx, "name").await? {
                    po_vals.insert(
                        "origin".into(),
                        Cell::Str(Arc::from(format!("Alternative to {orig_name}"))),
                    );
                }
                let alt = env
                    .records("purchase.order")?
                    .create(tx, vec![po_vals])
                    .await?;
                let alt_id = alt.ensure_one()?.int()?;

                if copy_products {
                    let rows = tx
                        .query(
                            "SELECT product_id::int8, product_qty, uom_id::int8 \
                             FROM purchase_order_line WHERE order_id = $1 AND product_id IS NOT NULL ORDER BY sequence, id",
                            &[&origin_id],
                        )
                        .await?;
                    let mut new_lines = Vec::with_capacity(rows.len());
                    for row in rows {
                        let pid: i64 = row.try_get(0).map_err(db)?;
                        let qty: Decimal = row.try_get(1).map_err(db)?;
                        let uom: Option<i64> = row.try_get(2).map_err(db)?;
                        let mut lv = RawVals::new();
                        lv.insert("order_id".into(), Cell::Id(alt_id));
                        lv.insert("product_id".into(), Cell::Id(pid));
                        lv.insert("product_qty".into(), Cell::Decimal(qty));
                        if let Some(u) = uom {
                            lv.insert("uom_id".into(), Cell::Id(u));
                        }
                        new_lines.push(lv);
                    }
                    if !new_lines.is_empty() {
                        env.records("purchase.order.line")?
                            .create(tx, new_lines)
                            .await?;
                    }
                }

                wizard.as_records().unlink(tx).await?;
                Ok(alt_id)
            })
            .await?;
        Ok(Value::Json(Arc::new(json!({
            "type": "ir.actions.act_window",
            "name": "Alternative RFQ",
            "res_model": "purchase.order",
            "res_id": alt_id,
            "views": [[false, "form"]],
            "target": "current",
        }))))
    }
}

#[methods(PurchaseOrder, addon = "purchase")]
impl PurchaseOrder {
    /// Extension hook invoked after an RFQ is emailed (bridges such as `purchase_mail` override
    /// this to post to chatter).
    #[method]
    async fn _on_rfq_emailed(
        _this: &RecordSet<PurchaseOrder>,
        _tx: &mut Transaction,
        _email_to: String,
        _subject: String,
        _body: String,
    ) -> Result<()> {
        Ok(())
    }

    /// Counts sibling alternative RFQs sharing `rfq_group_key` that are visible to the caller.
    #[compute(depends = ["rfq_group_key"], depends_context = ["uid", "allowed_company_ids"])]
    async fn _compute_alternative_count(
        this: &RecordSet<PurchaseOrder>,
        tx: &mut Transaction,
        out: &mut Assign<'_, PurchaseOrder>,
    ) -> Result<()> {
        tx.flush().await?;
        let keys = this.mapped(tx, PurchaseOrder::RFQ_GROUP_KEY).await?;
        let non_empty: Vec<String> = keys
            .iter()
            .flatten()
            .filter(|k| !k.trim().is_empty())
            .map(|k| k.to_string())
            .collect();
        let mut counts = std::collections::BTreeMap::<String, i32>::new();
        if !non_empty.is_empty() {
            let visible = this
                .env()
                .records("purchase.order")?
                .search(
                    tx,
                    &Domain::condition("rfq_group_key", Operator::In, non_empty)?,
                )
                .await?;
            let visible_ids = visible.ids();
            if !visible_ids.is_empty() {
                let rows = tx
                    .query(
                        "SELECT rfq_group_key::text, COUNT(*)::int4 \
                         FROM purchase_order WHERE id = ANY($1) GROUP BY rfq_group_key",
                        &[&visible_ids],
                    )
                    .await?;
                for row in rows {
                    counts.insert(row.try_get(0).map_err(db)?, row.try_get(1).map_err(db)?);
                }
            }
        }
        for (i, key) in keys.iter().enumerate() {
            let total = key
                .as_deref()
                .and_then(|k| counts.get(k).copied())
                .unwrap_or(0);
            let siblings = (total - 1).max(0);
            out.set(i, PurchaseOrder::ALTERNATIVE_COUNT, siblings)?;
        }
        Ok(())
    }

    /// Opens the Send RFQ by Email modal wizard.
    #[method]
    #[rpc(effect = read)]
    pub async fn action_open_rfq_send(
        this: &RecordSet<PurchaseOrder>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let rec = this.ensure_one()?;
        rec.as_records().check_access(tx, Operation::Write).await?;
        let oid = rec.as_records().ids()[0];
        Ok(Value::Json(Arc::new(json!({
            "type": "ir.actions.act_window",
            "name": "Send RFQ by Email",
            "res_model": "purchase.rfq.send",
            "target": "new",
            "views": [[false, "form"]],
            "context": {"default_order_id": oid},
        }))))
    }

    /// Marks the vendor receipt reminder as logged for a confirmed purchase order.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_send_receipt_reminder(
        this: &RecordSet<PurchaseOrder>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let rec = this.ensure_one()?;
        rec.as_records().check_access(tx, Operation::Write).await?;
        tx.flush().await?;
        rec.as_records().lock_for_update(tx, true).await?;
        let state = rec.get(tx, PurchaseOrder::STATE).await?;
        if state.as_deref() != Some("purchase") {
            return Err(Error::user(
                "Receipt reminders can only be logged for confirmed purchase orders.",
            ));
        }
        rec.as_records()
            .write_base(
                tx,
                RawVals::new()
                    .set_cell("receipt_reminder_email", Cell::Bool(true))
                    .set_cell("mail_reminder_confirmed", Cell::Bool(true)),
            )
            .await?;
        Ok(Value::Json(Arc::new(json!({
            "type": "ir.actions.client",
            "tag": "display_notification",
            "params": {
                "message": "Receipt reminder logged for vendor.",
                "type": "success",
            },
        }))))
    }

    /// Opens the Create Alternative RFQ wizard.
    #[method]
    #[rpc(effect = read)]
    pub async fn action_open_alternative_wizard(
        this: &RecordSet<PurchaseOrder>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let rec = this.ensure_one()?;
        rec.as_records().check_access(tx, Operation::Write).await?;
        let oid = rec.as_records().ids()[0];
        Ok(Value::Json(Arc::new(json!({
            "type": "ir.actions.act_window",
            "name": "Create Alternative RFQ",
            "res_model": "purchase.alternative.wizard",
            "target": "new",
            "views": [[false, "form"]],
            "context": {"default_origin_po_id": oid},
        }))))
    }

    /// Opens a side-by-side comparison of order lines across all visible alternative RFQs in this group.
    #[method]
    #[rpc(effect = read)]
    pub async fn action_compare_alternatives(
        this: &RecordSet<PurchaseOrder>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let rec = this.ensure_one()?;
        rec.as_records().check_access(tx, Operation::Read).await?;
        let key = rec
            .get(tx, PurchaseOrder::RFQ_GROUP_KEY)
            .await?
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                Error::user("This RFQ has no alternatives yet. Click Create Alternative first.")
            })?;
        let visible = rec
            .env()
            .records("purchase.order")?
            .search(
                tx,
                &Domain::condition("rfq_group_key", Operator::Eq, key.as_ref())?,
            )
            .await?;
        let po_ids = visible.ids();
        let views = tx
            .query(
                "SELECT res_id::int8 FROM ir_model_data \
                 WHERE module = 'purchase' AND name = 'view_purchase_order_line_compare' AND model = 'ir.ui.view'",
                &[],
            )
            .await?;
        let [view_row] = views.as_slice() else {
            return Err(Error::invariant(
                "The purchase alternative comparison view (purchase.view_purchase_order_line_compare) is missing.",
            ));
        };
        let vid: i64 = view_row.try_get(0).map_err(db)?;
        let view_spec = json!([[vid, "list"], [false, "form"]]);
        Ok(Value::Json(Arc::new(json!({
            "type": "ir.actions.act_window",
            "name": "Compare Alternative Order Lines",
            "res_model": "purchase.order.line",
            "views": view_spec,
            "domain": [["order_id", "in", po_ids]],
            "target": "current",
        }))))
    }
}

#[methods(PurchaseOrderLine, addon = "purchase")]
impl PurchaseOrderLine {
    /// Opens the parent Purchase Order from a comparison line.
    #[method]
    #[rpc(effect = read)]
    pub async fn action_open_purchase_order(
        this: &RecordSet<PurchaseOrderLine>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let line = this.ensure_one()?;
        let oid = line
            .get(tx, PurchaseOrderLine::ORDER_ID)
            .await?
            .ok_or_else(|| Error::missing("Line has no purchase order."))?
            .get();
        Ok(Value::Json(Arc::new(json!({
            "type": "ir.actions.act_window",
            "name": "Purchase Order",
            "res_model": "purchase.order",
            "res_id": oid,
            "views": [[false, "form"]],
            "target": "current",
        }))))
    }
}

#[cfg(test)]
mod tests {
    use super::select_best_quote_line;
    use erp_sdk::Decimal;
    use std::str::FromStr;

    #[test]
    fn best_quote_line_picks_lowest_exact_subtotal() {
        let quotes = vec![
            (10, Decimal::from_str("245.50").unwrap()),
            (11, Decimal::from_str("219.95").unwrap()),
            (12, Decimal::from_str("219.95").unwrap()),
        ];
        assert_eq!(select_best_quote_line(&quotes), Some(11));
        assert_eq!(select_best_quote_line(&[]), None);
    }
}
