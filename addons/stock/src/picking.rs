// SPDX-License-Identifier: MIT OR Apache-2.0
//! Transfers (`stock.picking`): table only. The
//! name sequence draw, the state evaluation from the counter vector and the counter maintenance are
//! 's and's; nothing here computes them.

use erp_sdk::DefaultContext;
use erp_sdk::prelude::*;

fn now(_ctx: &DefaultContext) -> Value {
    Value::DateTime(DateTime::now())
}

fn current_user(ctx: &DefaultContext) -> Value {
    Value::Int(ctx.uid)
}

/// `stock.picking`: a transfer, the document that groups moves of one operation type.
///
/// The name is unique per company whatever the operation type and has
/// no default until the numbering slice draws it from the type's sequence.
/// Company scope: its own company only; the
/// operation type, locations, partners and self links are checked by the framework default.
#[model(
    name = "stock.picking",
    description = "Transfer",
    order = "priority desc, scheduled_date asc, id desc",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    keys = [key(
        name = "_name_uniq",
        fields = ["company_id", "name"],
        message = "Reference must be unique per company!"
    )],
    constraints = [
        (
            "_counts_nonneg",
            "CHECK (move_count >= 0 AND draft_count >= 0 AND waiting_count >= 0 AND confirmed_count >= 0 \
             AND partially_available_count >= 0 AND assigned_count >= 0 AND done_count >= 0 \
             AND cancel_count >= 0 AND done_scrap_count >= 0 AND cancel_scrap_count >= 0 \
             AND assigned_zero_demand_count >= 0 AND open_positive_demand_count >= 0 \
             AND confirmed_positive_demand_count >= 0 AND mto_nonscrap_count >= 0)",
            "The move counts of a transfer cannot be negative."
        ),
        (
            "_counts_sum",
            "CHECK (draft_count + waiting_count + confirmed_count + partially_available_count \
             + assigned_count + done_count + cancel_count = move_count)",
            "The state counts of a transfer must add up to its move count."
        ),
        (
            "_counts_subsets",
            "CHECK (done_scrap_count <= done_count AND cancel_scrap_count <= cancel_count \
             AND assigned_zero_demand_count <= assigned_count \
             AND confirmed_positive_demand_count <= confirmed_count \
             AND confirmed_positive_demand_count <= open_positive_demand_count \
             AND open_positive_demand_count <= move_count - done_count - cancel_count \
             AND mto_nonscrap_count <= move_count - done_scrap_count - cancel_scrap_count)",
            "A subset count of a transfer cannot exceed the count it belongs to."
        ),
        (
            "_finite_shipping_weight",
            "CHECK (shipping_weight <> 'NaN' AND shipping_weight <> 'Infinity' AND shipping_weight <> '-Infinity')",
            "The shipping weight of a transfer must be a finite number."
        ),
    ],
    // the open transfers of a type, as the dashboard counts them (plan I2 §3.8); the benefit is
    // unmeasured (DEFERRED_PERFORMANCE)
    indexes = [index(
        name = "_open_type_state",
        fields = ["picking_type_id", "state"],
        filter = "[('state', 'not in', ['done', 'cancel'])]"
    )],
)]
pub struct StockPicking {
    /// Transfer reference, unique per company.
    #[field(
        char,
        required = true,
        copy = false,
        index = "trigram",
        string = "Reference"
    )]
    pub name: Option<Arc<str>>,
    /// Source document.
    #[field(char, index = "trigram", string = "Source Document")]
    pub origin: Option<Arc<str>>,
    /// Notes.
    #[field(html, string = "Notes")]
    pub note: Option<Arc<str>>,
    /// Transfer this one is the backorder of.
    #[field(
        many2one,
        comodel = "stock.picking",
        copy = false,
        readonly = true,
        index = "btree_not_null",
        string = "Back Order of"
    )]
    pub backorder_id: Option<RecId>,
    /// Backorders of this transfer.
    #[field(
        one2many,
        comodel = "stock.picking",
        inverse_name = "backorder_id",
        string = "Back Orders"
    )]
    pub backorder_ids: Vec<RecId>,
    /// Transfer this one returns.
    #[field(
        many2one,
        comodel = "stock.picking",
        copy = false,
        readonly = true,
        index = "btree_not_null",
        string = "Return of"
    )]
    pub return_id: Option<RecId>,
    /// Returns of this transfer.
    #[field(
        one2many,
        comodel = "stock.picking",
        inverse_name = "return_id",
        string = "Returns"
    )]
    pub return_ids: Vec<RecId>,
    /// Shipping policy; computed from the type.
    #[field(
        selection,
        selection = [
            ("direct", "As soon as possible, with back orders"),
            ("one", "When all products are ready")
        ],
        required = true,
        default = "direct",
        string = "Shipping Policy"
    )]
    pub move_type: Option<Arc<str>>,
    /// Status, evaluated from the counter vector.
    #[field(
        selection,
        selection = [
            ("draft", "Draft"),
            ("waiting", "Waiting Another Operation"),
            ("confirmed", "Waiting"),
            ("assigned", "Ready"),
            ("done", "Done"),
            ("cancel", "Cancelled")
        ],
        required = true,
        readonly = true,
        copy = false,
        default = "draft",
        string = "Status"
    )]
    pub state: Option<Arc<str>>,
    /// References of its moves (the other side of `stock_reference_picking_rel`).
    #[field(
        many2many,
        comodel = "stock.reference",
        relation = "stock_reference_picking_rel",
        column1 = "picking_id",
        column2 = "reference_id",
        readonly = true,
        string = "References"
    )]
    pub reference_ids: Vec<RecId>,
    /// Reservation priority.
    #[field(
        selection,
        selection = [("0", "Normal"), ("1", "Urgent")],
        default = "0",
        string = "Priority"
    )]
    pub priority: Option<Arc<str>>,
    /// Scheduled date of the first part of the shipment.
    #[field(datetime, index = true, default_fn = now, string = "Scheduled Date")]
    pub scheduled_date: Option<DateTime>,
    /// Deadline.
    #[field(datetime, string = "Deadline")]
    pub date_deadline: Option<DateTime>,
    /// Late or will be late against the deadline.
    #[field(boolean, default = false, string = "Is late")]
    pub has_deadline_issue: bool,
    /// Date the transfer was processed or cancelled.
    #[field(datetime, copy = false, readonly = true, string = "Date of Transfer")]
    pub date_done: Option<DateTime>,
    /// Source location.
    #[field(
        many2one,
        comodel = "stock.location",
        required = true,
        index = true,
        string = "Source Location"
    )]
    pub location_id: Option<RecId>,
    /// Destination location.
    #[field(
        many2one,
        comodel = "stock.location",
        required = true,
        index = true,
        string = "Destination Location"
    )]
    pub location_dest_id: Option<RecId>,
    #[field(
        one2many,
        comodel = "stock.move",
        inverse_name = "picking_id",
        domain = [("is_scrap", "=", false)],
        string = "Stock Moves"
    )]
    pub move_ids: Vec<RecId>,
    /// Operation type.
    #[field(
        many2one,
        comodel = "stock.picking.type",
        required = true,
        ondelete = "restrict",
        index = true,
        string = "Operation Type"
    )]
    pub picking_type_id: Option<RecId>,
    /// Contact.
    #[field(
        many2one,
        comodel = "res.partner",
        index = "btree_not_null",
        string = "Contact"
    )]
    pub partner_id: Option<RecId>,
    /// Owner assigned to the products on validation. `restrict` on delete, as the
    /// quant owner: the reference sets it null, which would strip the owner of a done transfer.
    #[field(
        many2one,
        comodel = "res.partner",
        ondelete = "restrict",
        index = "btree_not_null",
        string = "Assign Owner"
    )]
    pub owner_id: Option<RecId>,
    /// The reference key `(company_id, name)`
    /// leads with it, so it has no index of its own.
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        ondelete = "restrict",
        index = false,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Responsible user.
    #[field(
        many2one,
        comodel = "res.users",
        copy = false,
        default_fn = current_user,
        index = "btree_not_null",
        string = "Responsible"
    )]
    pub user_id: Option<RecId>,
    /// Printed.
    #[field(boolean, copy = false, string = "Printed")]
    pub printed: bool,
    /// Locked: demand (open) or done quantities (done) cannot be changed.
    #[field(boolean, default = true, copy = false)]
    pub is_locked: bool,
    /// Weight used for the shipping cost; computed by the delivery slice.
    #[field(float, digits = "Stock Weight", string = "Weight for Shipping")]
    pub shipping_weight: Decimal,
    /// Show the return button.
    #[field(boolean, default = true, string = "Show return button")]
    pub show_return: bool,
    /// Moves whose `picking_id` is this transfer, scrap included.
    #[field(integer, required = true, readonly = true, default = 0)]
    pub move_count: i32,
    /// Of them, in state draft.
    #[field(integer, required = true, readonly = true, default = 0)]
    pub draft_count: i32,
    /// Of them, waiting another move.
    #[field(integer, required = true, readonly = true, default = 0)]
    pub waiting_count: i32,
    /// Of them, confirmed.
    #[field(integer, required = true, readonly = true, default = 0)]
    pub confirmed_count: i32,
    /// Of them, partially available.
    #[field(integer, required = true, readonly = true, default = 0)]
    pub partially_available_count: i32,
    /// Of them, assigned.
    #[field(integer, required = true, readonly = true, default = 0)]
    pub assigned_count: i32,
    /// Of them, done.
    #[field(integer, required = true, readonly = true, default = 0)]
    pub done_count: i32,
    /// Of them, cancelled.
    #[field(integer, required = true, readonly = true, default = 0)]
    pub cancel_count: i32,
    /// Done scrap moves (within `done_count`).
    #[field(integer, required = true, readonly = true, default = 0)]
    pub done_scrap_count: i32,
    /// Cancelled scrap moves (within `cancel_count`).
    #[field(integer, required = true, readonly = true, default = 0)]
    pub cancel_scrap_count: i32,
    /// Assigned moves with zero demand, which leave the ranked state set (within `assigned_count`).
    #[field(integer, required = true, readonly = true, default = 0)]
    pub assigned_zero_demand_count: i32,
    /// Open moves with demand, read by the `one` policy (within `move_count`).
    #[field(integer, required = true, readonly = true, default = 0)]
    pub open_positive_demand_count: i32,
    /// Confirmed moves with demand, read by the `direct` policy (within `confirmed_count`).
    #[field(integer, required = true, readonly = true, default = 0)]
    pub confirmed_positive_demand_count: i32,
    /// Non-scrap make-to-order moves (3). A source that bypasses reservation gives
    /// `assigned` only while it is 0, that is, while every non-scrap move is make-to-stock.
    #[field(integer, required = true, readonly = true, default = 0)]
    pub mto_nonscrap_count: i32,
}
