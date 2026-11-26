// SPDX-License-Identifier: MIT OR Apache-2.0
//! Purchase Agreements (`purchase.requisition` & `purchase.requisition.line`): Blanket Orders and
//! Calls for Tenders / Purchase Templates with remaining quantity tracking and 1-click RFQ creation.

use std::collections::BTreeMap;

use erp_addon_uom::conversion::quantity_rounded;
use erp_addon_uom::models::load_unit;
use erp_sdk::money::{Increment, RoundingMode};
use erp_sdk::prelude::*;
use erp_sdk::sequence::draw_by_code;
use erp_sdk::serde_json::json;
use erp_sdk::{CompanyId, Operation, ValueMap};

use crate::order::strip_default_context;

/// Sequence code for Purchase Agreements (`data/purchase_data.xml`).
pub const REQUISITION_SEQUENCE_CODE: &str = "purchase.requisition";

fn db(err: impl std::fmt::Display) -> Error {
    Error::db(format!("Purchase agreement query failed: {err}"))
}

/// Pure exact-decimal remaining quantity for a blanket order line (never below zero).
pub fn compute_agreement_remaining_qty(agreed_qty: Decimal, ordered_qty: Decimal) -> Decimal {
    match agreed_qty.checked_sub(ordered_qty) {
        Some(diff) if diff > Decimal::ZERO => diff,
        _ => Decimal::ZERO,
    }
}

/// `purchase.requisition`: Blanket Order or Call for Tenders agreement with a vendor or multiple
/// vendors.
#[model(
    name = "purchase.requisition",
    description = "Purchase Agreement",
    order = "id desc",
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct PurchaseRequisition {
    /// Agreement reference number drawn from sequence.
    #[field(
        char,
        required = true,
        copy = false,
        default = "New",
        string = "Agreement"
    )]
    pub name: Option<Arc<str>>,
    /// Blanket order vs Call for Tenders / Purchase Template.
    #[field(
        selection,
        selection = [
            ("blanket_order", "Blanket Order"),
            ("purchase_template", "Call for Tenders"),
        ],
        required = true,
        default = "blanket_order",
        string = "Agreement Type"
    )]
    pub requisition_type: Option<Arc<str>>,
    /// Vendor (optional for Calls for Tenders, typical for Blanket Orders).
    #[field(
        many2one,
        comodel = "res.partner",
        check_company = true,
        string = "Vendor"
    )]
    pub vendor_id: Option<RecId>,
    /// Responsible buyer.
    #[field(
        many2one,
        comodel = "res.users",
        check_company = true,
        string = "Purchase Representative"
    )]
    pub user_id: Option<RecId>,
    /// Owning company.
    #[field(many2one, comodel = "res.company", required = true, string = "Company")]
    pub company_id: Option<RecId>,
    /// Company currency.
    #[field(
        many2one,
        comodel = "res.currency",
        related = "company_id.currency_id",
        store = true,
        string = "Currency"
    )]
    pub currency_id: Option<RecId>,
    /// External vendor contract reference.
    #[field(char, string = "Reference")]
    pub reference: Option<Arc<str>>,
    /// Agreement start date.
    #[field(date, string = "Start Date")]
    pub date_start: Option<Date>,
    /// Agreement expiration / deadline date.
    #[field(date, string = "Agreement Deadline")]
    pub date_end: Option<Date>,
    /// Lifecycle state.
    #[field(
        selection,
        selection = [
            ("draft", "Draft"),
            ("confirmed", "Confirmed"),
            ("done", "Closed"),
            ("cancel", "Cancelled"),
        ],
        required = true,
        readonly = true,
        default = "draft",
        copy = false,
        string = "Status"
    )]
    pub state: Option<Arc<str>>,
    /// Terms and notes.
    #[field(text, string = "Terms and Conditions")]
    pub description: Option<Arc<str>>,
    /// Agreement product lines.
    #[field(
        one2many,
        comodel = "purchase.requisition.line",
        inverse_name = "requisition_id",
        copy = true,
        string = "Products"
    )]
    pub line_ids: Vec<RecId>,
    /// Number of RFQs / Purchase Orders linked to this agreement.
    #[field(integer, compute = "_compute_order_count", string = "RFQs / Orders")]
    pub order_count: i32,
}

#[methods(PurchaseRequisition, addon = "purchase")]
impl PurchaseRequisition {
    #[method(override)]
    async fn default_get(
        this: &RecordSet<PurchaseRequisition>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<ValueMap> {
        let mut values: ValueMap = sup.call(tx, this.as_records(), (names.clone(),)).await?;
        let env = this.env().clone();
        if names.iter().any(|n| n == "user_id") && !values.contains_key("user_id") {
            values.insert("user_id".into(), Value::Int(env.uid()));
        }
        if names.iter().any(|n| n == "company_id")
            && !values.contains_key("company_id")
            && let Some(cid) = env.company_id()?
        {
            values.insert("company_id".into(), Value::Int(cid));
        }
        if names.iter().any(|n| n == "date_start") && !values.contains_key("date_start") {
            values.insert("date_start".into(), Value::Date(env.context_today()?));
        }
        Ok(values)
    }

    #[method(override)]
    async fn create(
        this: &RecordSet<PurchaseRequisition>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let env = this.env().clone();
        this.as_records()
            .check_access(tx, Operation::Create)
            .await?;
        if env.context().contains("default_state") {
            return Err(Error::validation(
                "The context key default_state is not accepted when creating a purchase agreement.",
            ));
        }
        let default_company = env.company_id()?;
        // Validate every row BEFORE drawing sequence numbers so invalid creates never burn sequences.
        let mut draw_companies: Vec<Option<i64>> = Vec::with_capacity(vals_list.len());
        for vals in &mut vals_list {
            if vals.contains("state") {
                return Err(Error::validation(
                    "Purchase agreement status changes only through its lifecycle actions.",
                ));
            }
            if let (Some(Cell::Date(ds)), Some(Cell::Date(de))) =
                (vals.get("date_start"), vals.get("date_end"))
                && ds > de
            {
                return Err(Error::validation(
                    "Agreement start date cannot be after the deadline date.",
                ));
            }
            if !vals.contains("company_id")
                && let Some(cid) = default_company
            {
                vals.insert("company_id".into(), Cell::Id(cid));
            }
            let unnamed = match vals.get("name") {
                None | Some(Cell::Null) => true,
                Some(Cell::Str(s)) => s.trim().is_empty() || s.as_ref() == "New",
                _ => false,
            };
            if unnamed {
                let cid = match vals.get("company_id") {
                    Some(Cell::Id(id)) => *id,
                    _ => default_company.ok_or_else(|| {
                        Error::validation("A purchase agreement needs a company.")
                    })?,
                };
                draw_companies.push(Some(cid));
            } else {
                draw_companies.push(None);
            }
        }
        let today = env.context_today()?;
        for (vals, cid_opt) in vals_list.iter_mut().zip(draw_companies) {
            if let Some(cid) = cid_opt {
                let company = CompanyId::new(cid)?;
                let seq = draw_by_code(
                    tx,
                    &env.sudo(true),
                    REQUISITION_SEQUENCE_CODE,
                    company,
                    Some(today),
                )
                .await?;
                vals.insert("name".into(), Cell::Str(seq.into()));
            }
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<PurchaseRequisition>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        if vals.contains("state") {
            return Err(Error::validation(
                "Purchase agreement status changes only through its lifecycle actions.",
            ));
        }
        if vals.contains("name") {
            return Err(Error::validation(
                "The agreement reference cannot be changed after creation.",
            ));
        }
        let records = this.as_records();
        if records.is_empty() || vals.is_empty() {
            return sup.call(tx, records, (vals,)).await;
        }
        let states = this.mapped(tx, PurchaseRequisition::STATE).await?;
        let starts = this.mapped(tx, PurchaseRequisition::DATE_START).await?;
        let ends = this.mapped(tx, PurchaseRequisition::DATE_END).await?;
        for (idx, st) in states.iter().enumerate() {
            match st.as_deref() {
                Some("done") => {
                    return Err(Error::validation(
                        "Closed purchase agreements cannot be modified.",
                    ));
                }
                Some("cancel") => {
                    return Err(Error::validation(
                        "Cancelled purchase agreements cannot be modified. Reset to draft first.",
                    ));
                }
                Some("confirmed") => {
                    for locked_field in ["company_id", "vendor_id", "requisition_type"] {
                        if vals.contains(locked_field) {
                            return Err(Error::validation(format!(
                                "Field '{locked_field}' cannot be changed on a confirmed purchase agreement."
                            )));
                        }
                    }
                }
                _ => {}
            }
            let new_start = match vals.get("date_start") {
                Some(Cell::Date(d)) => Some(*d),
                Some(Cell::Null) => None,
                _ => starts[idx],
            };
            let new_end = match vals.get("date_end") {
                Some(Cell::Date(d)) => Some(*d),
                Some(Cell::Null) => None,
                _ => ends[idx],
            };
            if let (Some(ds), Some(de)) = (new_start, new_end)
                && ds > de
            {
                return Err(Error::validation(
                    "Agreement start date cannot be after the deadline date.",
                ));
            }
        }
        sup.call(tx, records, (vals,)).await
    }

    #[method(override)]
    async fn unlink(
        this: &RecordSet<PurchaseRequisition>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        let states = this.mapped(tx, PurchaseRequisition::STATE).await?;
        for st in states {
            if !matches!(st.as_deref(), Some("draft" | "cancel")) {
                return Err(Error::validation(
                    "Only draft or cancelled purchase agreements can be deleted.",
                ));
            }
        }
        sup.call(tx, this.as_records(), ()).await
    }

    #[compute(depends = ["name"], depends_context = ["uid", "allowed_company_ids"])]
    async fn _compute_order_count(
        this: &RecordSet<PurchaseRequisition>,
        tx: &mut Transaction,
        out: &mut Assign<'_, PurchaseRequisition>,
    ) -> Result<()> {
        tx.flush().await?;
        let rec_ids = this.as_records().rec_ids();
        let real_ids: Vec<i64> = rec_ids
            .iter()
            .filter_map(|r| r.as_real().or(r.origin()).map(|id| id.get()))
            .collect();
        let mut counts: BTreeMap<i64, i32> = BTreeMap::new();
        if !real_ids.is_empty() {
            let visible = this
                .env()
                .records("purchase.order")?
                .search(
                    tx,
                    &Domain::condition("requisition_id", Operator::In, real_ids)?,
                )
                .await?;
            let vis_ids = visible.ids();
            if !vis_ids.is_empty() {
                let rows = tx
                    .query(
                        "SELECT requisition_id::int8, COUNT(*)::int4 \
                         FROM purchase_order WHERE id = ANY($1) GROUP BY requisition_id",
                        &[&vis_ids],
                    )
                    .await?;
                for row in rows {
                    counts.insert(row.try_get(0).map_err(db)?, row.try_get(1).map_err(db)?);
                }
            }
        }
        for (i, rec_id) in rec_ids.iter().enumerate() {
            let count = rec_id
                .as_real()
                .or(rec_id.origin())
                .and_then(|id| counts.get(&id.get()).copied())
                .unwrap_or(0);
            out.set(i, PurchaseRequisition::ORDER_COUNT, count)?;
        }
        Ok(())
    }

    #[method]
    #[rpc(effect = write)]
    pub async fn action_confirm(
        this: &RecordSet<PurchaseRequisition>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let rec = this.ensure_one()?;
        rec.as_records().check_access(tx, Operation::Write).await?;
        tx.flush().await?;
        rec.as_records().lock_for_update(tx, true).await?;
        if rec.get(tx, PurchaseRequisition::STATE).await?.as_deref() != Some("draft") {
            return Err(Error::validation(
                "Only draft purchase agreements can be confirmed.",
            ));
        }
        let rid = rec.as_records().ids()[0];
        let lines = tx
            .query(
                "SELECT 1 FROM purchase_requisition_line WHERE requisition_id = $1 LIMIT 1",
                &[&rid],
            )
            .await?;
        if lines.is_empty() {
            return Err(Error::user(
                "Add at least one product line before confirming a purchase agreement.",
            ));
        }
        rec.as_records()
            .write_base(
                tx,
                RawVals::new().set_cell("state", Cell::Str("confirmed".into())),
            )
            .await
    }

    #[method]
    #[rpc(effect = write)]
    pub async fn action_done(
        this: &RecordSet<PurchaseRequisition>,
        tx: &mut Transaction,
    ) -> Result<()> {
        this.as_records().check_access(tx, Operation::Write).await?;
        tx.flush().await?;
        this.as_records().lock_for_update(tx, true).await?;
        for st in this.mapped(tx, PurchaseRequisition::STATE).await? {
            if st.as_deref() != Some("confirmed") {
                return Err(Error::validation(
                    "Only confirmed purchase agreements can be closed.",
                ));
            }
        }
        this.as_records()
            .write_base(
                tx,
                RawVals::new().set_cell("state", Cell::Str("done".into())),
            )
            .await
    }

    #[method]
    #[rpc(effect = write)]
    pub async fn action_cancel(
        this: &RecordSet<PurchaseRequisition>,
        tx: &mut Transaction,
    ) -> Result<()> {
        this.as_records().check_access(tx, Operation::Write).await?;
        tx.flush().await?;
        this.as_records().lock_for_update(tx, true).await?;
        for st in this.mapped(tx, PurchaseRequisition::STATE).await? {
            if !matches!(st.as_deref(), Some("draft" | "confirmed")) {
                return Err(Error::validation(
                    "Only draft or confirmed purchase agreements can be cancelled.",
                ));
            }
        }
        this.as_records()
            .write_base(
                tx,
                RawVals::new().set_cell("state", Cell::Str("cancel".into())),
            )
            .await
    }

    #[method]
    #[rpc(effect = write)]
    pub async fn action_draft(
        this: &RecordSet<PurchaseRequisition>,
        tx: &mut Transaction,
    ) -> Result<()> {
        this.as_records().check_access(tx, Operation::Write).await?;
        tx.flush().await?;
        this.as_records().lock_for_update(tx, true).await?;
        for st in this.mapped(tx, PurchaseRequisition::STATE).await? {
            if st.as_deref() != Some("cancel") {
                return Err(Error::validation(
                    "Only cancelled purchase agreements can be reset to draft.",
                ));
            }
        }
        this.as_records()
            .write_base(
                tx,
                RawVals::new().set_cell("state", Cell::Str("draft".into())),
            )
            .await
    }

    /// Opens the RFQs / Purchase Orders created from this agreement.
    #[method]
    #[rpc(effect = read)]
    pub async fn action_view_orders(
        this: &RecordSet<PurchaseRequisition>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let rec = this.ensure_one()?;
        rec.as_records().check_access(tx, Operation::Read).await?;
        let rid = rec.as_records().ids()[0];
        Ok(Value::Json(Arc::new(json!({
            "type": "ir.actions.act_window",
            "name": "RFQs / Orders",
            "res_model": "purchase.order",
            // The web client takes a domain only from a result that opens in the page, with list then form.
            "target": "current",
            "views": [[false, "list"], [false, "form"]],
            "domain": [["requisition_id", "=", rid]],
            "context": {"default_requisition_id": rid},
        }))))
    }

    /// Creates a new Request for Quotation populated from this confirmed agreement's lines
    /// (remaining blanket quantities and agreed unit prices) and opens it.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_create_rfq(
        this: &RecordSet<PurchaseRequisition>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let rec = this.ensure_one()?;
        rec.as_records().check_access(tx, Operation::Read).await?;
        let rid = rec.as_records().ids()[0];

        let po_id = tx
            .savepoint(async |tx| {
                tx.flush().await?;
                rec.as_records().lock_for_update(tx, true).await?;
                let state = rec.get(tx, PurchaseRequisition::STATE).await?;
                if state.as_deref() != Some("confirmed") {
                    return Err(Error::user(
                        "Confirm the purchase agreement before creating a Request for Quotation.",
                    ));
                }
                let req_type = rec
                    .get(tx, PurchaseRequisition::REQUISITION_TYPE)
                    .await?
                    .unwrap_or_else(|| Arc::from("blanket_order"));
                let vendor_id = rec
                    .get(tx, PurchaseRequisition::VENDOR_ID)
                    .await?
                    .ok_or_else(|| {
                        Error::user(
                            "Select a Vendor on the purchase agreement before creating an RFQ.",
                        )
                    })?
                    .get();
                let company_id = rec
                    .get(tx, PurchaseRequisition::COMPANY_ID)
                    .await?
                    .ok_or_else(|| Error::invariant("Agreement has no company."))?
                    .get();
                let currency_id = rec.get(tx, PurchaseRequisition::CURRENCY_ID).await?;
                let ag_name = rec
                    .get(tx, PurchaseRequisition::NAME)
                    .await?
                    .unwrap_or_else(|| Arc::from("Agreement"));

                let env = strip_default_context(rec.env()).with_company(company_id)?;
                env.records("purchase.order")?
                    .check_access(tx, Operation::Create)
                    .await?;
                env.records("purchase.order.line")?
                    .check_access(tx, Operation::Create)
                    .await?;

                let mut po_vals = RawVals::new();
                po_vals.insert("partner_id".into(), Cell::Id(vendor_id));
                po_vals.insert("company_id".into(), Cell::Id(company_id));
                if let Some(cur) = currency_id {
                    po_vals.insert("currency_id".into(), Cell::Id(cur.get()));
                }
                po_vals.insert("origin".into(), Cell::Str(ag_name));
                po_vals.insert("requisition_id".into(), Cell::Id(rid));
                let po = env
                    .records("purchase.order")?
                    .create(tx, vec![po_vals])
                    .await?;
                let po_id = po.ensure_one()?.int()?;

                let line_rows = tx
                    .query(
                        "SELECT id::int8, product_id::int8, product_qty, product_uom_id::int8, price_unit \
                         FROM purchase_requisition_line WHERE requisition_id = $1 ORDER BY id",
                        &[&rid],
                    )
                    .await?;

                // Read already ordered quantities for this agreement across visible non-cancelled POs.
                let ord_rows = tx
                    .query(
                        "SELECT l.product_id::int8, l.product_qty, l.uom_id::int8 \
                         FROM purchase_order_line l \
                         JOIN purchase_order o ON o.id = l.order_id \
                         WHERE o.requisition_id = $1 AND o.company_id = $2 AND o.id != $3 \
                           AND o.state != 'cancel' AND l.product_id IS NOT NULL",
                        &[&rid, &company_id, &po_id],
                    )
                    .await?;
                let unit_digits = erp_addon_base::decimal_precision::precision_get(
                    &env,
                    tx,
                    "Product Unit",
                )
                .await?;
                let unit_inc = Increment::from_places(unit_digits)?;
                let mut unit_cache = BTreeMap::new();
                let mut ordered_raw: BTreeMap<i64, Vec<(Decimal, Option<i64>)>> = BTreeMap::new();
                for r in ord_rows {
                    let pid: i64 = r.try_get(0).map_err(db)?;
                    let pqty: Decimal = r.try_get(1).map_err(db)?;
                    let puom: Option<i64> = r.try_get(2).map_err(db)?;
                    ordered_raw.entry(pid).or_default().push((pqty, puom));
                }

                let mut po_lines = Vec::new();
                for row in line_rows {
                    let pid: i64 = row.try_get(1).map_err(db)?;
                    let agreed: Decimal = row.try_get(2).map_err(db)?;
                    let uom: Option<i64> = row.try_get(3).map_err(db)?;
                    let price: Decimal = row.try_get(4).map_err(db)?;
                    if price < Decimal::ZERO {
                        return Err(Error::validation(
                            "Purchase agreement line unit price cannot be negative.",
                        ));
                    }
                    let mut already = Decimal::ZERO;
                    if let Some(entries) = ordered_raw.get(&pid) {
                        for &(oq, ouom) in entries {
                            let converted = match (ouom, uom) {
                                (Some(src_id), Some(dst_id)) if src_id != dst_id => {
                                    if let std::collections::btree_map::Entry::Vacant(e) =
                                        unit_cache.entry(src_id)
                                    {
                                        e.insert(load_unit(&env, tx, src_id).await?);
                                    }
                                    if let std::collections::btree_map::Entry::Vacant(e) =
                                        unit_cache.entry(dst_id)
                                    {
                                        e.insert(load_unit(&env, tx, dst_id).await?);
                                    }
                                    quantity_rounded(
                                        &unit_cache[&src_id],
                                        &unit_cache[&dst_id],
                                        oq,
                                        &unit_inc,
                                        RoundingMode::HalfAwayFromZero,
                                    )?
                                }
                                _ => oq,
                            };
                            already += converted;
                        }
                    }
                    let qty = if req_type.as_ref() == "blanket_order" {
                        compute_agreement_remaining_qty(agreed, already)
                    } else {
                        agreed
                    };
                    let mut lv = RawVals::new();
                    lv.insert("order_id".into(), Cell::Id(po_id));
                    lv.insert("product_id".into(), Cell::Id(pid));
                    lv.insert("product_qty".into(), Cell::Decimal(qty));
                    if let Some(u) = uom {
                        lv.insert("uom_id".into(), Cell::Id(u));
                    }
                    lv.insert("price_unit".into(), Cell::Decimal(price));
                    po_lines.push(lv);
                }
                if !po_lines.is_empty() {
                    env.records("purchase.order.line")?
                        .create(tx, po_lines)
                        .await?;
                }
                Ok(po_id)
            })
            .await?;

        Ok(Value::Json(Arc::new(json!({
            "type": "ir.actions.act_window",
            "name": "Request for Quotation",
            "res_model": "purchase.order",
            "res_id": po_id,
            "views": [[false, "form"]],
            "target": "current",
        }))))
    }
}

/// `purchase.requisition.line`: product, agreed quantity, agreed price, ordered & remaining qty.
#[model(
    name = "purchase.requisition.line",
    description = "Purchase Agreement Line",
    order = "id",
    check_company_auto = true,
    company_access = parent(edge = "requisition_id")
)]
pub struct PurchaseRequisitionLine {
    /// Owning agreement.
    #[field(
        many2one,
        comodel = "purchase.requisition",
        required = true,
        ondelete = "cascade",
        string = "Agreement"
    )]
    pub requisition_id: Option<RecId>,
    /// Company from the agreement.
    #[field(
        many2one,
        comodel = "res.company",
        related = "requisition_id.company_id",
        store = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Currency from the agreement.
    #[field(
        many2one,
        comodel = "res.currency",
        related = "requisition_id.currency_id",
        store = true,
        string = "Currency"
    )]
    pub currency_id: Option<RecId>,
    /// Product covered by the agreement.
    #[field(
        many2one,
        comodel = "product.product",
        required = true,
        check_company = true,
        string = "Product"
    )]
    pub product_id: Option<RecId>,
    /// Agreed quantity.
    #[field(
        float,
        digits = "Product Unit",
        default = 1,
        required = true,
        string = "Quantity"
    )]
    pub product_qty: Decimal,
    /// Unit of measure.
    #[field(many2one, comodel = "uom.uom", string = "Unit")]
    pub product_uom_id: Option<RecId>,
    /// Agreed unit price.
    #[field(float, digits = "Product Price", default = 0, string = "Unit Price")]
    pub price_unit: Decimal,
    /// Quantity already ordered across non-cancelled RFQs / POs linked to this agreement.
    #[field(
        float,
        digits = "Product Unit",
        compute = "_compute_ordered_qty",
        string = "Ordered Qty"
    )]
    pub qty_ordered: Decimal,
    /// Quantity remaining under this agreement.
    #[field(
        float,
        digits = "Product Unit",
        compute = "_compute_ordered_qty",
        string = "Remaining Qty"
    )]
    pub qty_remaining: Decimal,
}

#[methods(PurchaseRequisitionLine, addon = "purchase")]
impl PurchaseRequisitionLine {
    #[constrains("price_unit", "product_qty")]
    async fn _check_non_negative(
        this: &RecordSet<PurchaseRequisitionLine>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let prices = this.mapped(tx, PurchaseRequisitionLine::PRICE_UNIT).await?;
        let qtys = this
            .mapped(tx, PurchaseRequisitionLine::PRODUCT_QTY)
            .await?;
        for (p, q) in prices.into_iter().zip(qtys) {
            if p < Decimal::ZERO {
                return Err(Error::validation(
                    "Purchase agreement unit price cannot be negative.",
                ));
            }
            if q < Decimal::ZERO {
                return Err(Error::validation(
                    "Purchase agreement quantity cannot be negative.",
                ));
            }
        }
        Ok(())
    }

    #[method(override)]
    async fn create(
        this: &RecordSet<PurchaseRequisitionLine>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let env = this.env().clone();
        for vals in &mut vals_list {
            if let Some(Cell::Id(rid)) = vals.get("requisition_id") {
                let st = env
                    .records("purchase.requisition")?
                    .browse([*rid])?
                    .cell(tx, "state")
                    .await?;
                if matches!(st, Cell::Str(ref s) if matches!(s.as_ref(), "done" | "cancel")) {
                    return Err(Error::validation(
                        "Cannot add lines to a closed or cancelled purchase agreement.",
                    ));
                }
            }
            if let Some(Cell::Id(pid)) = vals.get("product_id").cloned()
                && !vals.contains("product_uom_id")
            {
                match env
                    .records("product.product")?
                    .browse([pid])?
                    .cell(tx, "uom_id")
                    .await?
                {
                    Cell::Id(uid) => {
                        vals.insert("product_uom_id".into(), Cell::Id(uid));
                    }
                    Cell::Null => {}
                    other => {
                        return Err(Error::programming(format!(
                            "product.product.uom_id returned {other:?}"
                        )));
                    }
                }
            }
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<PurchaseRequisitionLine>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        let reqs = this
            .mapped(tx, PurchaseRequisitionLine::REQUISITION_ID)
            .await?;
        let req_ids: Vec<RecId> = reqs.into_iter().flatten().collect();
        if !req_ids.is_empty() {
            let states = this
                .env()
                .records("purchase.requisition")?
                .browse_ids(req_ids)
                .cells(tx, "state")
                .await?;
            if states
                .iter()
                .any(|c| matches!(c, Cell::Str(s) if matches!(s.as_ref(), "done" | "cancel")))
            {
                return Err(Error::validation(
                    "Lines of a closed or cancelled purchase agreement cannot be modified.",
                ));
            }
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }

    #[method(override)]
    async fn unlink(
        this: &RecordSet<PurchaseRequisitionLine>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        let reqs = this
            .mapped(tx, PurchaseRequisitionLine::REQUISITION_ID)
            .await?;
        let req_ids: Vec<RecId> = reqs.into_iter().flatten().collect();
        if !req_ids.is_empty() {
            let states = this
                .env()
                .records("purchase.requisition")?
                .browse_ids(req_ids)
                .cells(tx, "state")
                .await?;
            if states
                .iter()
                .any(|c| matches!(c, Cell::Str(s) if matches!(s.as_ref(), "done" | "cancel")))
            {
                return Err(Error::validation(
                    "Lines of a closed or cancelled purchase agreement cannot be deleted.",
                ));
            }
        }
        sup.call(tx, this.as_records(), ()).await
    }

    #[compute(
        depends = ["requisition_id", "product_id", "product_qty", "product_uom_id"],
        depends_context = ["uid", "allowed_company_ids"]
    )]
    async fn _compute_ordered_qty(
        this: &RecordSet<PurchaseRequisitionLine>,
        tx: &mut Transaction,
        out: &mut Assign<'_, PurchaseRequisitionLine>,
    ) -> Result<()> {
        tx.flush().await?;
        let env = this.env().clone();
        let reqs = this
            .mapped(tx, PurchaseRequisitionLine::REQUISITION_ID)
            .await?;
        let prods = this.mapped(tx, PurchaseRequisitionLine::PRODUCT_ID).await?;
        let qtys = this
            .mapped(tx, PurchaseRequisitionLine::PRODUCT_QTY)
            .await?;
        let line_uoms = this
            .mapped(tx, PurchaseRequisitionLine::PRODUCT_UOM_ID)
            .await?;
        let req_ids: Vec<i64> = reqs.iter().flatten().map(|r| r.get()).collect();
        type OrderedByKey = BTreeMap<(i64, i64), Vec<(Decimal, Option<i64>)>>;
        let mut by_key: OrderedByKey = BTreeMap::new();
        if !req_ids.is_empty() {
            let visible_orders = env
                .records("purchase.order")?
                .search(
                    tx,
                    &Domain::and([
                        Domain::condition("requisition_id", Operator::In, req_ids)?,
                        Domain::condition("state", Operator::Ne, "cancel")?,
                    ]),
                )
                .await?;
            let vis_ids = visible_orders.ids();
            if !vis_ids.is_empty() {
                let rows = tx
                    .query(
                        "SELECT o.requisition_id::int8, l.product_id::int8, l.product_qty, l.uom_id::int8 \
                         FROM purchase_order_line l \
                         JOIN purchase_order o ON o.id = l.order_id \
                         WHERE o.id = ANY($1) AND l.product_id IS NOT NULL",
                        &[&vis_ids],
                    )
                    .await?;
                for row in rows {
                    let rid: i64 = row.try_get(0).map_err(db)?;
                    let pid: i64 = row.try_get(1).map_err(db)?;
                    let qty: Decimal = row.try_get(2).map_err(db)?;
                    let uom: Option<i64> = row.try_get(3).map_err(db)?;
                    by_key.entry((rid, pid)).or_default().push((qty, uom));
                }
            }
        }
        let unit_digits =
            erp_addon_base::decimal_precision::precision_get(&env, tx, "Product Unit").await?;
        let unit_inc = Increment::from_places(unit_digits)?;
        let mut unit_cache = BTreeMap::new();
        for (i, (((req, prod), agreed), target_uom)) in reqs
            .iter()
            .zip(&prods)
            .zip(qtys)
            .zip(&line_uoms)
            .enumerate()
        {
            let mut ordered = Decimal::ZERO;
            if let (Some(r), Some(p)) = (req, prod)
                && let Some(entries) = by_key.get(&(r.get(), p.get()))
            {
                let dst = target_uom.map(RecId::get);
                for &(oq, ouom) in entries {
                    let converted = match (ouom, dst) {
                        (Some(src_id), Some(dst_id)) if src_id != dst_id => {
                            if let std::collections::btree_map::Entry::Vacant(e) =
                                unit_cache.entry(src_id)
                            {
                                e.insert(load_unit(&env, tx, src_id).await?);
                            }
                            if let std::collections::btree_map::Entry::Vacant(e) =
                                unit_cache.entry(dst_id)
                            {
                                e.insert(load_unit(&env, tx, dst_id).await?);
                            }
                            quantity_rounded(
                                &unit_cache[&src_id],
                                &unit_cache[&dst_id],
                                oq,
                                &unit_inc,
                                RoundingMode::HalfAwayFromZero,
                            )?
                        }
                        _ => oq,
                    };
                    ordered += converted;
                }
            }
            let remaining = compute_agreement_remaining_qty(agreed, ordered);
            out.set(i, PurchaseRequisitionLine::QTY_ORDERED, ordered)?;
            out.set(i, PurchaseRequisitionLine::QTY_REMAINING, remaining)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::compute_agreement_remaining_qty;
    use erp_sdk::Decimal;
    use std::str::FromStr;

    #[test]
    fn agreement_remaining_quantity_is_exact_and_non_negative() {
        assert_eq!(
            compute_agreement_remaining_qty(
                Decimal::from_str("100.50").unwrap(),
                Decimal::from_str("35.25").unwrap(),
            ),
            Decimal::from_str("65.25").unwrap()
        );
        assert_eq!(
            compute_agreement_remaining_qty(Decimal::from(10), Decimal::from(15)),
            Decimal::ZERO
        );
    }
}
