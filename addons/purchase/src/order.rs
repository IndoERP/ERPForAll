// SPDX-License-Identifier: MIT OR Apache-2.0
//! The state, the lock flag and the approval date change only through the operations
//! D5.3; a client write or create of them is refused by field name. Every operation takes the row
//! locks of its orders in id order, reads their state inside the transaction, refuses the whole
//! call when one order is not in a legal source state (naming the orders), and writes through the
//! base writer, so a `write` override never sees a state change. The extension steps are
//! `_approval_allowed`, `_approve`, `_action_cancel`, `_mark_rfqs_as_sent` and `_protected_fields`:
//! methods other addons override, not callable over RPC.

use std::collections::{BTreeMap, BTreeSet};

use erp_sdk::prelude::*;
use erp_sdk::sequence::draw_by_code;
use erp_sdk::{CompanyId, Operation, ValueMap};

use crate::MANAGER_GROUP;
use crate::line::{
    amount, in_company_currency, orders_without_product, reprice_orders, resync_orders,
};
use crate::settings::OrderPolicy;

/// The code of the order numbering sequence (default data `seq_purchase_order`).
pub const SEQUENCE_CODE: &str = "purchase.order";

/// The number a caller gives to ask for the next one, as the reference's default does.
pub const NEW_NAME: &str = "New";

/// The deadline too: it
/// sets the day the company-currency total converts at, which the approval question reads.
pub const CONFIRMED_FIELDS: [&str; 4] = ["partner_id", "currency_id", "company_id", "date_order"];

/// The posted vendor bills of the orders (a linked bill is always a vendor bill: the source guard
/// of `direct_bill_integrity.rs`): order id, order number, bill number, bill id, the first per
/// order first.
const POSTED_BILLS: &str = "SELECT DISTINCT o.id::int8, o.name, m.name, m.id::int8 FROM purchase_direct_bill_line k \
    JOIN purchase_order o ON o.id = k.order_id \
    JOIN account_move m ON m.id = k.bill_id \
    WHERE k.order_id = ANY($1) AND m.state = 'posted' ORDER BY 1, 3";

/// The draft vendor bills of the orders.
const DRAFT_BILLS: &str = "SELECT DISTINCT k.bill_id::int8 FROM purchase_direct_bill_line k \
    JOIN account_move m ON m.id = k.bill_id \
    WHERE k.order_id = ANY($1) AND m.state = 'draft' ORDER BY 1";

/// Fields only the operations write (5).
const OPERATION_FIELDS: [&str; 4] = ["state", "locked", "date_approve", "mail_reminder_confirmed"];

/// Fields a client write never sets: the operation fields and the number, given only at create.
const WRITE_REFUSED: [&str; 5] = [
    "state",
    "locked",
    "date_approve",
    "mail_reminder_confirmed",
    "name",
];

/// Returns a copy of `env` with `default_*` keys removed while preserving `tz`, `lang`,
/// `company_id`, `allowed_company_ids`, and all other caller context entries.
pub(crate) fn strip_default_context(env: &Env) -> Env {
    let mut ctx = env.context().clone();
    let keys: Vec<String> = ctx
        .default_field_names()
        .map(|k| format!("default_{k}"))
        .collect();
    for k in keys {
        ctx.remove(&k);
    }
    env.with_context_replaced(ctx)
}

/// Fields computed from the lines; never written by a client.
const AMOUNT_FIELDS: [&str; 6] = [
    "amount_untaxed",
    "amount_tax",
    "amount_total",
    "amount_total_cc",
    "date_planned",
    "invoice_status",
];

/// A change of these fields on an order prices its lines' amounts again; the company too, so the
/// total in the company currency follows the new company.
const RESYNC_FIELDS: [&str; 3] = ["currency_id", "date_order", "company_id"];

/// `purchase.order`: vendor, company, currency, deadline, buyer and the lifecycle state.
#[model(
    name = "purchase.order",
    description = "Purchase Order",
    order = "id desc",
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
)]
pub struct PurchaseOrder {
    /// The order number, drawn from the `purchase.order` sequence at creation.
    #[field(char, required = true, copy = false, string = "Order Reference")]
    pub name: Option<Arc<str>>,
    /// The document that caused this order, for people to read; never matched.
    #[field(char, copy = false, string = "Source")]
    pub origin: Option<Arc<str>>,
    /// The date by which the request should be confirmed; now when none is given.
    #[field(
        datetime,
        required = true,
        copy = false,
        index = true,
        string = "Order Deadline"
    )]
    pub date_order: Option<DateTime>,
    /// When the order was approved; written by the approval step only.
    #[field(
        datetime,
        readonly = true,
        copy = false,
        index = true,
        string = "Order Date"
    )]
    pub date_approve: Option<DateTime>,
    /// The vendor.
    #[field(
        many2one,
        comodel = "res.partner",
        required = true,
        check_company = true,
        ondelete = "restrict",
        index = true,
        string = "Vendor"
    )]
    pub partner_id: Option<RecId>,
    /// The order currency; the company's when none is given.
    #[field(
        many2one,
        comodel = "res.currency",
        required = true,
        string = "Currency"
    )]
    pub currency_id: Option<RecId>,
    /// RFQ, RFQ sent, to approve, purchase order or cancelled; only the operations change it.
    #[field(
        selection,
        selection = [
            ("draft", "RFQ"),
            ("sent", "RFQ Sent"),
            ("to approve", "To Approve"),
            ("purchase", "Purchase Order"),
            ("cancel", "Cancelled")
        ],
        required = true,
        readonly = true,
        copy = false,
        default = "draft",
        index = true,
        string = "Status"
    )]
    pub state: Option<Arc<str>>,
    /// A locked order cannot be cancelled and keeps its protected fields; lock and unlock change it.
    #[field(
        boolean,
        default = false,
        copy = false,
        readonly = true,
        string = "Locked"
    )]
    pub locked: bool,
    /// The buyer; the creating user when none is given.
    #[field(
        many2one,
        comodel = "res.users",
        check_company = true,
        index = true,
        string = "Buyer"
    )]
    pub user_id: Option<RecId>,
    /// The company; the active one when none is given.
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        index = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// The company currency, read through the company.
    #[field(
        many2one,
        comodel = "res.currency",
        related = "company_id.currency_id",
        string = "Company Currency"
    )]
    pub company_currency_id: Option<RecId>,
    /// The lines; copied with the order.
    #[field(
        one2many,
        comodel = "purchase.order.line",
        inverse_name = "order_id",
        copy = true,
        string = "Order Lines"
    )]
    pub order_line: Vec<RecId>,
    /// The earliest expected arrival of the lines.
    #[field(
        datetime,
        readonly = true,
        copy = false,
        index = true,
        string = "Expected Arrival"
    )]
    pub date_planned: Option<DateTime>,
    /// The sum of the line subtotals.
    #[field(
        monetary,
        currency_field = "currency_id",
        readonly = true,
        copy = false,
        string = "Untaxed Amount"
    )]
    pub amount_untaxed: Decimal,
    /// The sum of the line taxes.
    #[field(
        monetary,
        currency_field = "currency_id",
        readonly = true,
        copy = false,
        string = "Taxes"
    )]
    pub amount_tax: Decimal,
    /// The untaxed amount plus the taxes.
    #[field(
        monetary,
        currency_field = "currency_id",
        readonly = true,
        copy = false,
        string = "Total"
    )]
    pub amount_total: Decimal,
    /// How many vendor bills and refunds of the order the caller may read
    /// (`crate::bill_buttons`).
    #[field(integer, compute = "_compute_invoice_count", string = "Bill Count")]
    pub invoice_count: i32,
    /// True while Cancel would be refused: a vendor bill of the order is posted, or a bridge's
    /// downstream document holds it (`_cancel_blocked`). The form hides Cancel with it.
    #[field(
        boolean,
        compute = "_compute_cancel_blocked",
        string = "Cancel Blocked"
    )]
    pub cancel_blocked: bool,
    /// The total converted to the company currency at the deadline's date; the approval
    /// question compares it with the company's minimum amount (BUG-132).
    #[field(
        monetary,
        currency_field = "company_currency_id",
        readonly = true,
        copy = false,
        string = "Total in Company Currency"
    )]
    pub amount_total_cc: Decimal,
    /// Non-blocking vendor, parent company, and product notices for this order. Computed elevated,
    /// then limited to the partners and products the caller's own rights can read: a record the
    /// caller cannot read adds no message.
    #[field(
        text,
        compute = "_compute_purchase_warning_text",
        compute_sudo = true,
        readonly = true,
        copy = false,
        string = "Order notices"
    )]
    pub purchase_warning_text: Option<Arc<str>>,
    /// Nothing to bill, waiting bills or fully billed, from the lines of a confirmed order
    /// ([`crate::billing`]).
    #[field(
        selection,
        selection = [
            ("no", "Nothing to Bill"),
            ("to invoice", "Waiting Bills"),
            ("invoiced", "Fully Billed"),
        ],
        compute = "_compute_invoice_status",
        store = true,
        string = "Billing Status"
    )]
    pub invoice_status: Option<Arc<str>>,
    /// Blanket Order or Call for Tenders agreement this RFQ / PO belongs to.
    #[field(
        many2one,
        comodel = "purchase.requisition",
        check_company = true,
        ondelete = "set null",
        index = true,
        string = "Purchase Agreement"
    )]
    pub requisition_id: Option<RecId>,
    /// Shared comparison group key linking alternative RFQs for vendor comparison.
    #[field(char, copy = false, index = true, string = "Alternative Group")]
    pub rfq_group_key: Option<Arc<str>>,
    /// Number of sibling alternative RFQs in the same comparison group.
    #[field(
        integer,
        compute = "_compute_alternative_count",
        string = "Alternatives"
    )]
    pub alternative_count: i32,
    /// Automatically remind the vendor before expected receipt date.
    #[field(boolean, default = false, string = "Ask Confirmation")]
    pub receipt_reminder_email: bool,
    /// Number of days before receipt to send the reminder.
    #[field(integer, default = 1, string = "Days Before Receipt")]
    pub reminder_date_before_receipt: i32,
    /// Whether the receipt confirmation reminder has been sent.
    #[field(
        boolean,
        default = false,
        readonly = true,
        copy = false,
        string = "Reminder Sent"
    )]
    pub mail_reminder_confirmed: bool,
}

#[methods(PurchaseOrder, addon = "purchase")]
impl PurchaseOrder {
    /// The values a new order form starts with: the active company, its currency, the user as
    /// buyer and now as deadline, each only when asked for and not already given
    /// (the vendor's
    /// purchase currency comes with the order defaults slice).
    #[method(override)]
    async fn default_get(
        this: &RecordSet<PurchaseOrder>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<ValueMap> {
        let env = this.env().clone();
        let wanted = |values: &ValueMap, field: &str| {
            names.iter().any(|n| n == field) && !values.contains_key(field)
        };
        let mut values: ValueMap = sup.call(tx, this.as_records(), (names.clone(),)).await?;
        if wanted(&values, "user_id") {
            values.insert("user_id".into(), Value::Int(env.uid()));
        }
        if wanted(&values, "date_order") {
            values.insert("date_order".into(), Value::DateTime(DateTime::now()));
        }
        if wanted(&values, "receipt_reminder_email") {
            let remind = crate::settings::default_receipt_reminder(tx).await?;
            values.insert("receipt_reminder_email".into(), Value::Bool(remind));
        }
        let company = match values.get("company_id") {
            Some(Value::Int(company)) => Some(*company),
            _ => env.company_id()?,
        };
        let Some(company) = company else {
            return Ok(values);
        };
        if wanted(&values, "company_id") {
            values.insert("company_id".into(), Value::Int(company));
        }
        if wanted(&values, "currency_id") {
            let currency = env
                .records("res.company")?
                .browse([company])?
                .cell(tx, "currency_id")
                .await?;
            values.insert("currency_id".into(), Value::from_cell(&currency));
        }
        Ok(values)
    }

    /// Refuses a state, lock or approval date (V7), fills company, currency, buyer and
    /// deadline, and draws the number of an order given none.
    #[method(override)]
    async fn create(
        this: &RecordSet<PurchaseOrder>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let env = this.env().clone();
        // the create right first: the number is drawn from a shared counter below
        this.as_records()
            .check_access(tx, Operation::Create)
            .await?;
        refuse_context_defaults(&env, &OPERATION_FIELDS, "purchase order")?;
        refuse_context_defaults(&env, &AMOUNT_FIELDS, "purchase order")?;
        // The lines of the `order_line` commands are created under this context too.
        refuse_context_defaults(&env, &crate::line::COMPUTED, "purchase order")?;
        refuse_context_defaults(&env, &crate::line::RECEIVED, "purchase order")?;
        for vals in &mut vals_list {
            refuse_fields(vals, &OPERATION_FIELDS)?;
            refuse_fields(vals, &AMOUNT_FIELDS)?;
            fill_defaults(tx, &env, vals).await?;
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }

    /// Refuses the operation fields and the number, then the frozen fields of confirmed,
    /// cancelled and locked orders, naming the orders.
    #[method(override)]
    async fn write(
        this: &RecordSet<PurchaseOrder>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        // the write right first: what follows reads numbers and states of the caller's ids
        if !this.as_records().is_empty() {
            this.as_records().check_access(tx, Operation::Write).await?;
        }
        refuse_fields(&vals, &WRITE_REFUSED)?;
        refuse_fields(&vals, &AMOUNT_FIELDS)?;
        let resync = RESYNC_FIELDS.iter().any(|f| vals.contains(f));
        let written: Vec<String> = vals
            .iter()
            .map(|(name, _)| name)
            .chain(vals.iter_commands().map(|(name, _)| name))
            .map(str::to_owned)
            .collect();
        if !written.is_empty() && !this.as_records().is_empty() {
            let protected: Vec<String> =
                this.as_records().call(tx, "_protected_fields", ()).await?;
            refuse_frozen(tx, &this.ids(), &written, &protected).await?;
            if ["partner_id", "currency_id", "company_id"]
                .iter()
                .any(|f| vals.contains(f))
            {
                protect_billed_identity(tx, this.as_records(), &vals).await?;
            }
            if let Some(company) = vals.get("company_id") {
                refuse_company_change(tx, &this.ids(), id_cell(Some(company), "company_id")?)
                    .await?;
            }
        }
        sup.call(tx, this.as_records(), (vals,)).await?;
        let env = this.env().clone();
        let orders: BTreeSet<i64> = this.ids().into_iter().collect();
        // a new vendor prices the lines again, as the reference's price computation reads it
        if written.iter().any(|f| f.as_str() == "partner_id") && !orders.is_empty() {
            reprice_orders(tx, &env, &orders).await?;
        }
        if resync && !orders.is_empty() {
            resync_orders(tx, &env, &orders).await?;
        }
        Ok(())
    }

    /// Only cancelled orders are deleted; any other refuses the whole call.
    #[method(override)]
    async fn unlink(
        this: &RecordSet<PurchaseOrder>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        if !this.as_records().is_empty() {
            this.as_records()
                .check_access(tx, Operation::Unlink)
                .await?;
            let heads = lock_heads(tx, this.as_records()).await?;
            let open = labels(&heads, |h| h.state != "cancel");
            if !open.is_empty() {
                return Err(Error::user(format!(
                    "Cancel a purchase order before deleting it: {open}."
                )));
            }
        }
        sup.call(tx, this.as_records(), ()).await
    }

    /// Confirms requests for quotation: the approval step for each order the approval question
    /// admits, "to approve" for the others.
    #[method]
    #[rpc(effect = write)]
    pub async fn button_confirm(
        this: &RecordSet<PurchaseOrder>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let Some((orders, heads)) = begin(tx, this.as_records()).await? else {
            return Ok(());
        };
        refuse_states(&heads, &["draft", "sent"], "confirm")?;
        let env = this.env().clone();
        let incomplete = orders_without_product(tx, &env, &sorted_ids(&orders)).await?;
        if !incomplete.is_empty() {
            return Err(Error::user(format!(
                "Some order lines have no product; give them one before confirming: {}.",
                labels(&heads, |h| incomplete.contains(&h.id))
            )));
        }
        let allowed: Vec<bool> = orders.call(tx, "_approval_allowed", ()).await?;
        let (approve, wait) = split(&heads, &allowed)?;
        if !wait.is_empty() {
            orders
                .browse(wait)?
                .write_base(
                    tx,
                    RawVals::new().set_cell("state", Cell::Str("to approve".into())),
                )
                .await?;
        }
        if !approve.is_empty() {
            let () = orders.browse(approve)?.call(tx, "_approve", ()).await?;
        }
        Ok(())
    }

    /// Approves orders waiting for approval; a caller the approval question does not admit is
    /// refused.
    #[method]
    #[rpc(effect = write)]
    pub async fn button_approve(
        this: &RecordSet<PurchaseOrder>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let Some((orders, heads)) = begin(tx, this.as_records()).await? else {
            return Ok(());
        };
        refuse_states(&heads, &["to approve"], "approve")?;
        let allowed: Vec<bool> = orders.call(tx, "_approval_allowed", ()).await?;
        let (_, refused) = split(&heads, &allowed)?;
        if !refused.is_empty() {
            return Err(Error::access(format!(
                "Approving these purchase orders needs the purchase administrator group \
                 ({MANAGER_GROUP}): {}.",
                labels(&heads, |h| refused.contains(&h.id))
            )));
        }
        orders.call(tx, "_approve", ()).await
    }

    /// Cancels orders; a locked order refuses the whole call, an order already cancelled is left
    /// as it is.
    #[method]
    #[rpc(effect = write)]
    pub async fn button_cancel(
        this: &RecordSet<PurchaseOrder>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let Some((orders, heads)) = begin(tx, this.as_records()).await? else {
            return Ok(());
        };
        let locked = labels(&heads, |h| h.locked);
        if !locked.is_empty() {
            return Err(Error::user(format!(
                "Unlock these purchase orders before cancelling them: {locked}."
            )));
        }
        let open: Vec<i64> = heads
            .iter()
            .filter(|h| h.state != "cancel")
            .map(|h| h.id)
            .collect();
        if open.is_empty() {
            return Ok(());
        }
        orders.browse(open)?.call(tx, "_action_cancel", ()).await
    }

    /// Returns cancelled orders to requests for quotation.
    #[method]
    #[rpc(effect = write)]
    pub async fn button_draft(this: &RecordSet<PurchaseOrder>, tx: &mut Transaction) -> Result<()> {
        let Some((orders, heads)) = begin(tx, this.as_records()).await? else {
            return Ok(());
        };
        refuse_states(&heads, &["cancel"], "set to draft")?;
        orders
            .write_base(
                tx,
                RawVals::new().set_cell("state", Cell::Str("draft".into())),
            )
            .await
    }

    /// Locks confirmed orders; an order already locked is left as it is (
    #[method]
    #[rpc(effect = write)]
    pub async fn button_lock(this: &RecordSet<PurchaseOrder>, tx: &mut Transaction) -> Result<()> {
        let Some((orders, heads)) = begin(tx, this.as_records()).await? else {
            return Ok(());
        };
        refuse_states(&heads, &["purchase"], "lock")?;
        set_locked(tx, &orders, &heads, true).await
    }

    /// Unlocks orders; purchase administrators only, checked before anything else
    /// (BUG-131).
    #[method]
    #[rpc(effect = write)]
    pub async fn button_unlock(
        this: &RecordSet<PurchaseOrder>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let env = this.env().clone();
        if !env.su() && !tx.has_group(&env, MANAGER_GROUP).await? {
            return Err(Error::access(format!(
                "Unlocking a purchase order needs the purchase administrator group ({MANAGER_GROUP})."
            )));
        }
        let Some((orders, heads)) = begin(tx, this.as_records()).await? else {
            return Ok(());
        };
        set_locked(tx, &orders, &heads, false).await
    }

    /// The approval question, one answer per order in id order: may the caller approve it? One
    /// approval level, a total under the company's minimum amount, or a purchase administrator.
    /// Overridden by an addon that
    /// adds an approval rule; never callable over RPC.
    #[method]
    async fn _approval_allowed(
        this: &RecordSet<PurchaseOrder>,
        tx: &mut Transaction,
    ) -> Result<Vec<bool>> {
        let heads = read_heads(tx, &sorted_ids(this.as_records())).await?;
        let env = this.env().clone();
        let manager = env.su() || tx.has_group(&env, MANAGER_GROUP).await?;
        let policies = policies(tx, &heads).await?;
        let totals = totals_in_company_currency(tx, &env, &heads).await?;
        Ok(heads
            .iter()
            .zip(totals)
            .map(|(head, total)| policies[&head.company].approves(total, manager))
            .collect())
    }

    /// The approval step: the orders become purchase orders with today's approval date, and are
    /// locked when their company locks confirmed orders. The one extension
    /// point for what happens when an order is confirmed (receipts: `purchase_stock`).
    #[method]
    async fn _approve(this: &RecordSet<PurchaseOrder>, tx: &mut Transaction) -> Result<()> {
        let orders = this.as_records();
        // Controlling & Budget Availability Control (AVC): validate Cost Center locks and
        // record Purchase Commitments before approving orders.
        let order_ids = sorted_ids(orders);
        if !order_ids.is_empty() {
            sync_order_commitments(tx, orders.env(), &order_ids).await?;
        }
        orders
            .write_base(
                tx,
                RawVals::new()
                    .set_cell("state", Cell::Str("purchase".into()))
                    .set_cell("date_approve", Cell::DateTime(DateTime::now())),
            )
            .await?;
        let heads = read_heads(tx, &order_ids).await?;
        let policies = policies(tx, &heads).await?;
        let lock: Vec<i64> = heads
            .iter()
            .filter(|h| policies[&h.company].lock)
            .map(|h| h.id)
            .collect();
        if lock.is_empty() {
            return Ok(());
        }
        orders
            .browse(lock)?
            .write_base(tx, RawVals::new().set_cell("locked", Cell::Bool(true)))
            .await
    }

    /// The cancel step: an order with a posted vendor bill is refused, otherwise its draft bills
    /// are cancelled and the state is written. Bridges extend this step, never the action, so no
    /// extension runs twice for one order (V9). The reference also refuses an order with a bill
    /// that is neither a draft nor cancelled. The write right on the orders comes first: the
    /// refusal describes their bills, and names one only when the caller may read it.
    #[method]
    async fn _action_cancel(this: &RecordSet<PurchaseOrder>, tx: &mut Transaction) -> Result<()> {
        let records = this.as_records();
        records.check_access(tx, Operation::Write).await?;
        tx.flush().await?;
        if let Some(row) = tx.query(POSTED_BILLS, &[&records.ids()]).await?.first() {
            let order: String = row.try_get(1).map_err(db)?;
            let number: Option<String> = row.try_get(2).map_err(db)?;
            let bill: i64 = row.try_get(3).map_err(db)?;
            let readable = !records
                .env()
                .records("account.move")?
                .browse([bill])?
                .filtered_access(tx, Operation::Read)
                .await?
                .is_empty();
            let named = match number {
                Some(number) if readable => format!(" {number}"),
                _ => String::new(),
            };
            return Err(Error::user(format!(
                "The purchase order {order} cannot be cancelled: its vendor bill{named} is posted, and a posted bill stays on its order. Correct the bill with a refund instead."
            )));
        }
        let drafts: Vec<i64> = tx
            .query(DRAFT_BILLS, &[&records.ids()])
            .await?
            .iter()
            .map(|row| row.try_get(0).map_err(db))
            .collect::<Result<_>>()?;
        erp_addon_account::moves::cancel_document_drafts(tx, records.env(), &drafts).await?;
        let po_line_ids: Vec<i64> = tx
            .query(
                "SELECT id::int8 FROM purchase_order_line WHERE order_id = ANY($1::int8[])",
                &[&records.ids()],
            )
            .await?
            .iter()
            .map(|r| r.try_get(0).map_err(db))
            .collect::<Result<_>>()?;
        erp_addon_account::budget::avc::release_commitments_for_source(
            tx,
            "purchase.order.line",
            &po_line_ids,
        )
        .await?;
        records
            .write_base(
                tx,
                RawVals::new().set_cell("state", Cell::Str("cancel".into())),
            )
            .await
    }

    /// The orders whose Cancel would be refused: in the base, those with a posted vendor bill or
    /// refund. A bridge that refuses the cancel step adds its own through an override that calls
    /// `sup`.
    #[method]
    async fn _cancel_blocked(
        this: &RecordSet<PurchaseOrder>,
        tx: &mut Transaction,
    ) -> Result<Records> {
        let records = this.as_records();
        tx.flush().await?;
        let mut blocked = BTreeSet::new();
        for row in tx.query(POSTED_BILLS, &[&records.ids()]).await? {
            blocked.insert(row.try_get::<_, i64>(0).map_err(db)?);
        }
        records.browse(blocked)
    }

    #[compute(depends = [])]
    async fn _compute_cancel_blocked(
        this: &RecordSet<PurchaseOrder>,
        tx: &mut Transaction,
        out: &mut Assign<'_, PurchaseOrder>,
    ) -> Result<()> {
        let records = this.as_records();
        // an onchange snapshot is a new record whose origin is the stored order
        let row = |id: &RecId| id.as_real().or(id.origin()).map(|id| id.get());
        let blocked: Records = records
            .browse(records.rec_ids().iter().filter_map(row))?
            .call(tx, "_cancel_blocked", ())
            .await?;
        let blocked: BTreeSet<i64> = blocked.ids().into_iter().collect();
        for (index, id) in records.rec_ids().iter().enumerate() {
            let held = row(id).is_some_and(|id| blocked.contains(&id));
            out.set(index, PurchaseOrder::CANCEL_BLOCKED, held)?;
        }
        Ok(())
    }

    /// Marks the draft orders of the call as sent and leaves the others as they are; called after
    /// an RFQ is sent or printed (the mail and print slices), never over RPC.
    #[method]
    async fn _mark_rfqs_as_sent(
        this: &RecordSet<PurchaseOrder>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let Some((orders, heads)) = begin(tx, this.as_records()).await? else {
            return Ok(());
        };
        let drafts: Vec<i64> = heads
            .iter()
            .filter(|h| h.state == "draft")
            .map(|h| h.id)
            .collect();
        if drafts.is_empty() {
            return Ok(());
        }
        orders
            .browse(drafts)?
            .write_base(
                tx,
                RawVals::new().set_cell("state", Cell::Str("sent".into())),
            )
            .await
    }

    /// The fields a locked order keeps (5). The base holds vendor, currency and
    /// company; the lines slice and the bridges add theirs by overriding this method.
    #[method]
    async fn _protected_fields(
        this: &RecordSet<PurchaseOrder>,
        tx: &mut Transaction,
    ) -> Result<Vec<String>> {
        let _ = (this, tx);
        Ok(CONFIRMED_FIELDS
            .iter()
            .chain(&["order_line"])
            .map(|f| (*f).to_owned())
            .collect())
    }
}

/// What an operation reads of one order before it decides.
struct Head {
    id: i64,
    name: String,
    state: String,
    locked: bool,
    company: i64,
}

fn sorted_ids(orders: &Records) -> Vec<i64> {
    let mut ids = orders.ids();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// Write access, then the row locks in id order (never waiting: a held lock fails at once,
/// .3 rule 1), then the headers read inside the transaction. `None` for no order.
async fn begin(tx: &mut Transaction, orders: &Records) -> Result<Option<(Records, Vec<Head>)>> {
    if orders.is_empty() {
        return Ok(None);
    }
    orders.check_access(tx, Operation::Write).await?;
    let heads = lock_heads(tx, orders).await?;
    let sorted = orders.browse(heads.iter().map(|h| h.id))?;
    Ok(Some((sorted, heads)))
}

async fn lock_heads(tx: &mut Transaction, orders: &Records) -> Result<Vec<Head>> {
    let ids = sorted_ids(orders);
    tx.flush().await?;
    orders
        .browse(ids.iter().copied())?
        .lock_for_update(tx, true)
        .await?;
    read_heads(tx, &ids).await
}

async fn read_heads(tx: &mut Transaction, ids: &[i64]) -> Result<Vec<Head>> {
    tx.flush().await?;
    let rows = tx
        .query(
            "SELECT id::int8, name::text, state::text, coalesce(locked, false), company_id::int8 \
             FROM purchase_order WHERE id = ANY($1::int8[]) ORDER BY id",
            &[&ids],
        )
        .await?;
    if rows.len() != ids.len() {
        return Err(Error::missing(
            "Some of the purchase orders no longer exist.",
        ));
    }
    rows.iter()
        .map(|row| {
            Ok(Head {
                id: row.try_get(0).map_err(db)?,
                name: row.try_get(1).map_err(db)?,
                state: row.try_get(2).map_err(db)?,
                locked: row.try_get(3).map_err(db)?,
                company: row.try_get(4).map_err(db)?,
            })
        })
        .collect()
}

/// The orders of `heads` that pass `pick`, as `number (state)`, comma-separated.
fn labels(heads: &[Head], pick: impl Fn(&Head) -> bool) -> String {
    heads
        .iter()
        .filter(|h| pick(h))
        .map(|h| format!("{} ({})", h.name, h.state))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Refuses the whole call when an order is not in one of `legal` (3 rule 2).
fn refuse_states(heads: &[Head], legal: &[&str], operation: &str) -> Result<()> {
    let bad = labels(heads, |h| !legal.contains(&h.state.as_str()));
    if bad.is_empty() {
        return Ok(());
    }
    Err(Error::validation(format!(
        "You can {operation} a purchase order only in the state {}; these are not: {bad}.",
        legal.join(" or ")
    )))
}

/// The ids of `heads` the approval question admitted, and the others.
fn split(heads: &[Head], allowed: &[bool]) -> Result<(Vec<i64>, Vec<i64>)> {
    if allowed.len() != heads.len() {
        return Err(Error::internal(format!(
            "_approval_allowed answered for {} of {} purchase orders.",
            allowed.len(),
            heads.len()
        )));
    }
    let (yes, no): (Vec<_>, Vec<_>) = heads.iter().zip(allowed).partition(|(_, ok)| **ok);
    Ok((
        yes.into_iter().map(|(h, _)| h.id).collect(),
        no.into_iter().map(|(h, _)| h.id).collect(),
    ))
}

/// Lock or unlock the orders whose flag differs; the others are already in their end state and
/// nothing is written for them.
async fn set_locked(
    tx: &mut Transaction,
    orders: &Records,
    heads: &[Head],
    lock: bool,
) -> Result<()> {
    let change: Vec<i64> = heads
        .iter()
        .filter(|h| h.locked != lock)
        .map(|h| h.id)
        .collect();
    if change.is_empty() {
        return Ok(());
    }
    orders
        .browse(change)?
        .write_base(tx, RawVals::new().set_cell("locked", Cell::Bool(lock)))
        .await
}

/// The order policy of each company of `heads`, one settings read per company.
async fn policies(tx: &mut Transaction, heads: &[Head]) -> Result<BTreeMap<i64, OrderPolicy>> {
    let mut policies = BTreeMap::new();
    for head in heads {
        if let std::collections::btree_map::Entry::Vacant(slot) = policies.entry(head.company) {
            slot.insert(OrderPolicy::read(tx, head.company).await?);
        }
    }
    Ok(policies)
}

/// The total of each order converted now, in this transaction, from the order currency to its
/// company's currency at the deadline's date (never the active company's, BUG-132), in `heads`
/// order. The stored `amount_total_cc` is not trusted here: the question reads what it compares.
async fn totals_in_company_currency(
    tx: &mut Transaction,
    env: &Env,
    heads: &[Head],
) -> Result<Vec<Decimal>> {
    let orders = env
        .records("purchase.order")?
        .browse(heads.iter().map(|h| h.id))?;
    let totals = orders.cells(tx, "amount_total").await?;
    let currencies = orders.cells(tx, "currency_id").await?;
    let deadlines = orders.cells(tx, "date_order").await?;
    let mut out = Vec::with_capacity(heads.len());
    for (i, head) in heads.iter().enumerate() {
        let total = amount(totals.get(i).unwrap_or(&Cell::Null), "amount_total")?;
        let currency = id_cell(currencies.get(i), "currency_id")?;
        let company = RecId::from_real(head.company);
        let (Some(currency), Some(company)) = (currency.and_then(RecId::from_real), company) else {
            return Err(Error::invariant(format!(
                "purchase order {} has no company or currency",
                head.id
            )));
        };
        let deadline = match deadlines.get(i) {
            Some(Cell::DateTime(at)) => Some(*at),
            _ => None,
        };
        let company_env = env.with_company(head.company)?;
        let converted =
            in_company_currency(tx, &company_env, total, currency, company, deadline).await?;
        out.push(converted);
    }
    Ok(out)
}

/// Refuses a `default_<field>` context key for any of `fields`, by name. The base create applies
/// context defaults after this addon's create read the values, so such a key would go around the
/// refusal of the same field in the values (an order born confirmed, a line born received).
pub(crate) fn refuse_context_defaults(env: &Env, fields: &[&str], what: &str) -> Result<()> {
    match fields
        .iter()
        .find(|f| env.context().contains(&format!("default_{f}")))
    {
        None => Ok(()),
        Some(field) => Err(Error::validation(format!(
            "The context key default_{field} is not accepted when a {what} is created; the field \
             {field} is set by the application."
        ))),
    }
}

/// Refuses any of `fields` present in `vals`, by name (5, ruling A16).
fn refuse_fields(vals: &RawVals, fields: &[&str]) -> Result<()> {
    match fields.iter().find(|f| vals.contains(f)) {
        None => Ok(()),
        Some(field) => Err(Error::validation(format!(
            "The field {field} of a purchase order is set by its operations (Confirm, Approve, \
             Cancel, Set to Draft, Lock, Unlock) or at creation, not by a write."
        ))),
    }
}

/// Refuses a write to a cancelled order (any field), to a confirmed one (vendor, currency,
/// company) and to a locked one (the protected fields), naming orders and fields (V5, V6, V3).
async fn refuse_frozen(
    tx: &mut Transaction,
    ids: &[i64],
    written: &[String],
    protected: &[String],
) -> Result<()> {
    let heads = read_heads(tx, &{
        let mut ids = ids.to_vec();
        ids.sort_unstable();
        ids.dedup();
        ids
    })
    .await?;
    let mut refusals = Vec::new();
    for head in &heads {
        let frozen: Vec<&str> = written
            .iter()
            .map(String::as_str)
            .filter(|f| {
                head.state == "cancel"
                    || (head.state == "purchase" && CONFIRMED_FIELDS.contains(f))
                    || (head.locked && protected.iter().any(|p| p == f))
            })
            .collect();
        if !frozen.is_empty() {
            refusals.push(format!(
                "{} ({}): {}",
                head.name,
                head.state,
                frozen.join(", ")
            ));
        }
    }
    if refusals.is_empty() {
        return Ok(());
    }
    Err(Error::validation(format!(
        "These fields of confirmed, cancelled or locked purchase orders cannot change: {}.",
        refusals.join("; ")
    )))
}

/// Preserve source attribution across cancellation and redrafting of a directly billed order.
async fn protect_billed_identity(
    tx: &mut Transaction,
    orders: &Records,
    vals: &RawVals,
) -> Result<()> {
    for field in ["partner_id", "currency_id", "company_id"] {
        if vals.contains(field)
            && !tx
                .has_field_access(
                    orders.env(),
                    "purchase.order",
                    field,
                    erp_sdk::FieldOp::Write,
                )
                .await?
        {
            return Err(Error::access("You cannot edit this purchase order field."));
        }
    }
    let partner = id_cell(vals.get("partner_id"), "partner_id")?;
    let currency = id_cell(vals.get("currency_id"), "currency_id")?;
    let company = id_cell(vals.get("company_id"), "company_id")?;
    let found = tx.query(
        "SELECT 1 FROM purchase_direct_bill_line s JOIN account_move m ON m.id=s.bill_id
         WHERE s.order_id=ANY($1) AND (($2 AND m.partner_id IS DISTINCT FROM $3)
             OR ($4 AND m.currency_id IS DISTINCT FROM $5) OR ($6 AND m.company_id IS DISTINCT FROM $7)) LIMIT 1",
        &[&orders.ids(), &vals.contains("partner_id"), &partner,
            &vals.contains("currency_id"), &currency, &vals.contains("company_id"), &company],
    ).await?;
    if !found.is_empty() {
        return Err(Error::validation(
            "A purchase order linked to a vendor bill must keep its supplier, currency and company.",
        ));
    }
    Ok(())
}

/// Refuses a move to another company while the order has lines: their taxes and prices belong to
/// the company they were made for, and are not made again for the new one (V10). A
/// confirmed order is refused by `refuse_frozen`.
async fn refuse_company_change(
    tx: &mut Transaction,
    ids: &[i64],
    new_company: Option<i64>,
) -> Result<()> {
    let mut ids = ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    let heads = read_heads(tx, &ids).await?;
    for head in heads.iter().filter(|h| Some(h.company) != new_company) {
        let rows = tx
            .query(
                "SELECT EXISTS (SELECT 1 FROM purchase_order_line WHERE order_id = $1)",
                &[&head.id],
            )
            .await?;
        let has_lines: bool = rows
            .first()
            .ok_or_else(|| Error::internal("The lines of a purchase order could not be counted."))?
            .try_get(0)
            .map_err(db)?;
        if has_lines {
            return Err(Error::validation(format!(
                "The purchase order {} has lines, whose taxes and prices belong to its company, so \
                 it cannot move to another company. Remove the lines first, or make the order in \
                 the other company.",
                head.name
            )));
        }
    }
    Ok(())
}

fn id_cell(cell: Option<&Cell>, field: &str) -> Result<Option<i64>> {
    match cell {
        None | Some(Cell::Null | Cell::Bool(false)) => Ok(None),
        Some(Cell::Id(id)) => Ok(Some(*id)),
        Some(Cell::Int(id)) => Ok(Some(i64::from(*id))),
        Some(other) => Err(Error::validation(format!(
            "{field} takes a record id, not {other:?}."
        ))),
    }
}

/// A `default_<field>` context key for the company, currency, buyer or deadline is the value of a
/// new order that is given none: this create reads those values before the base would apply the
/// key, so it takes the key itself and never ignores it.
fn adopt_context_defaults(env: &Env, vals: &mut RawVals) -> Result<()> {
    for field in ["company_id", "currency_id", "user_id", "date_order"] {
        if vals.contains(field) {
            continue;
        }
        let key = format!("default_{field}");
        let cell = match env.context().get(&key) {
            None | Some(Value::False | Value::Bool(false)) => continue,
            Some(Value::Int(id)) if field != "date_order" => Cell::Id(id),
            Some(Value::DateTime(at)) if field == "date_order" => Cell::DateTime(at),
            Some(other) => {
                return Err(Error::validation(format!(
                    "{key} takes {}, not {other:?}.",
                    if field == "date_order" {
                        "a date and time"
                    } else {
                        "a record id"
                    }
                )));
            }
        };
        vals.insert(field.into(), cell);
    }
    Ok(())
}

/// Company (the active one), currency (the company's), buyer (the user) and deadline (now) of a
/// new order, each only when not given (
/// `:174-177`, `:519-526`), then the number when none is given (`:455-467`,.7).
async fn fill_defaults(tx: &mut Transaction, env: &Env, vals: &mut RawVals) -> Result<()> {
    adopt_context_defaults(env, vals)?;
    let company = match id_cell(vals.get("company_id"), "company_id")? {
        Some(company) => company,
        None => {
            let company = env.company_id()?.ok_or_else(|| {
                Error::user("No company is active: give the purchase order a company.")
            })?;
            vals.insert("company_id".into(), Cell::Id(company));
            company
        }
    };
    if id_cell(vals.get("currency_id"), "currency_id")?.is_none() {
        let currency = env
            .records("res.company")?
            .browse([company])?
            .cell(tx, "currency_id")
            .await?;
        vals.insert("currency_id".into(), currency);
    }
    if !vals.contains("user_id") {
        vals.insert("user_id".into(), Cell::Id(env.uid()));
    }
    if matches!(
        vals.get("date_order"),
        None | Some(Cell::Null | Cell::Bool(false))
    ) {
        vals.insert("date_order".into(), Cell::DateTime(DateTime::now()));
    }
    let unnamed = match vals.get("name") {
        None | Some(Cell::Null | Cell::Bool(false)) => true,
        Some(Cell::Str(name)) => name.as_ref() == NEW_NAME,
        Some(_) => false,
    };
    if unnamed {
        // the counter period follows the deadline's date in the user's time zone (`:461-465`)
        let date = match vals.get("date_order") {
            Some(Cell::DateTime(at)) => Date::context_today_at(env.tz(), *at)
                .map_err(|e| Error::internal(e.message().to_owned()))?,
            other => {
                return Err(Error::programming(format!(
                    "date_order of a new purchase order is not a date and time: {other:?}"
                )));
            }
        };
        // the draw puts the order's company first; a company the caller is not in is refused
        let name =
            draw_by_code(tx, env, SEQUENCE_CODE, CompanyId::new(company)?, Some(date)).await?;
        vals.insert("name".into(), Cell::Str(name.into()));
    }
    Ok(())
}

/// Validates Cost Center commitment locks and records/updates real-time Budget AVC Purchase
/// Commitments for all lines of `order_ids` in exact company currency.
pub(crate) async fn sync_order_commitments(
    tx: &mut Transaction,
    env: &Env,
    order_ids: &[i64],
) -> Result<()> {
    if order_ids.is_empty() {
        return Ok(());
    }
    tx.flush().await?;
    let line_rows = tx
        .query(
            "SELECT l.id::int8, o.name::text, o.company_id::int8, o.currency_id::int8, \
                    o.date_order, l.analytic_account_id::int8, l.asset_id::int8, \
                    COALESCE(l.price_subtotal, 0), a.account_asset_id::int8, \
                    a.company_id::int8 \
             FROM purchase_order_line l \
             JOIN purchase_order o ON o.id = l.order_id \
             LEFT JOIN account_asset a ON a.id = l.asset_id \
             WHERE l.order_id = ANY($1::int8[]) \
             ORDER BY l.id",
            &[&order_ids],
        )
        .await?;
    let mut requests = Vec::new();
    let mut referenced_assets = Vec::new();
    for r in &line_rows {
        if let Some(aid) = r.try_get::<_, Option<i64>>(6).map_err(db)? {
            referenced_assets.push(aid);
        }
    }
    if !referenced_assets.is_empty() {
        referenced_assets.sort_unstable();
        referenced_assets.dedup();
        env.records("account.asset")?
            .browse(referenced_assets)?
            .check_access(tx, Operation::Read)
            .await?;
    }
    for r in line_rows {
        let lid: i64 = r.try_get(0).map_err(db)?;
        let oname: String = r.try_get(1).map_err(db)?;
        let cid: i64 = r.try_get(2).map_err(db)?;
        let cur_id: i64 = r.try_get(3).map_err(db)?;
        let date_order: Option<DateTime> = r.try_get(4).map_err(db)?;
        let cc_id: Option<i64> = r.try_get(5).map_err(db)?;
        let asset_id: Option<i64> = r.try_get(6).map_err(db)?;
        let subtotal: Decimal = r.try_get(7).map_err(db)?;
        let asset_acc: Option<i64> = r.try_get(8).map_err(db)?;
        let asset_company: Option<i64> = r.try_get(9).map_err(db)?;

        if asset_id.is_some() && asset_company != Some(cid) {
            return Err(Error::validation(
                "CIP Asset on a purchase order line must belong to the same company as the order.",
            ));
        }
        let odate = match date_order {
            Some(dt) => dt.date(),
            None => erp_addon_base::company_calendar::business_date(tx, cid).await?,
        };
        if let Some(cc) = cc_id {
            erp_addon_account::controlling::validate_center_for_commitment(tx, cc, odate).await?;
        }
        if cc_id.is_none() && asset_id.is_none() {
            continue;
        }
        if subtotal <= Decimal::ZERO {
            continue;
        }
        let cur_rec = RecId::from_real(cur_id)
            .ok_or_else(|| Error::invariant(format!("Invalid order currency {cur_id}.")))?;
        let co_rec = RecId::from_real(cid)
            .ok_or_else(|| Error::invariant(format!("Invalid order company {cid}.")))?;
        let co_env = env.with_company(cid)?;
        let cc_amount =
            in_company_currency(tx, &co_env, subtotal, cur_rec, co_rec, date_order).await?;
        if cc_amount > Decimal::ZERO {
            requests.push(erp_addon_account::budget::avc::CommitmentRequest {
                source_model: "purchase.order.line".to_owned(),
                source_id: lid,
                source_ref: oname,
                company_id: cid,
                account_id: asset_acc,
                analytic_account_id: cc_id,
                asset_id,
                date: odate,
                amount: cc_amount,
            });
        }
    }
    if !requests.is_empty() {
        erp_addon_account::budget::avc::check_and_record_commitments(tx, env, &requests).await?;
    }
    Ok(())
}

fn db(error: impl std::fmt::Display) -> Error {
    Error::db(format!("A purchase order could not be read: {error}"))
}
