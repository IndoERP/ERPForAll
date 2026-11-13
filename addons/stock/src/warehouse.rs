// SPDX-License-Identifier: MIT OR Apache-2.0
//! Warehouses and operation types: tables only. Generating a warehouse's
//! locations, types, routes and rules, the environment defaults and the step changes belong to;
//! numbering to (topology ADR A1).

use erp_sdk::prelude::*;

/// `stock.warehouse`: a site with its generated topology.
#[model(
    name = "stock.warehouse",
    description = "Warehouse",
    order = "sequence, id",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    constraints = [
        (
            "_warehouse_name_uniq",
            "UNIQUE (name, company_id)",
            "The name of the warehouse must be unique per company!"
        ),
        (
            "_warehouse_code_uniq",
            "UNIQUE (code, company_id)",
            "The short name of the warehouse must be unique per company!"
        ),
    ],
)]
pub struct StockWarehouse {
    /// Warehouse name, unique per company.
    #[field(char, required = true, string = "Warehouse")]
    pub name: Option<Arc<str>>,
    /// Archived warehouses keep their topology for history.
    #[field(boolean, default = true)]
    pub active: bool,
    /// Owning company.
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        readonly = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Address.
    #[field(many2one, comodel = "res.partner", string = "Address")]
    pub partner_id: Option<RecId>,
    /// Root view location of the warehouse.
    #[field(
        many2one,
        comodel = "stock.location",
        required = true,
        index = true,
        string = "View Location"
    )]
    pub view_location_id: Option<RecId>,
    /// Main stock location.
    #[field(
        many2one,
        comodel = "stock.location",
        required = true,
        string = "Location Stock"
    )]
    pub lot_stock_id: Option<RecId>,
    /// Short name, unique per company; prefixes generated names.
    #[field(char, required = true, string = "Short Name")]
    pub code: Option<Arc<str>>,
    /// Routes products follow through this warehouse by default.
    #[field(
        many2many,
        comodel = "stock.route",
        relation = "stock_route_warehouse",
        column1 = "warehouse_id",
        column2 = "route_id",
        copy = false,
        string = "Routes"
    )]
    pub route_ids: Vec<RecId>,
    /// Incoming flow.
    #[field(
        selection,
        selection = [
            ("one_step", "Receive and Store (1 step)"),
            ("two_steps", "Receive then Store (2 steps)"),
            ("three_steps", "Receive, Quality Control, then Store (3 steps)")
        ],
        default = "one_step",
        required = true,
        string = "Incoming Shipments"
    )]
    pub reception_steps: Option<Arc<str>>,
    /// Outgoing flow.
    #[field(
        selection,
        selection = [
            ("ship_only", "Deliver (1 step)"),
            ("pick_ship", "Pick then Deliver (2 steps)"),
            ("pick_pack_ship", "Pick, Pack, then Deliver (3 steps)")
        ],
        default = "ship_only",
        required = true,
        string = "Outgoing Shipments"
    )]
    pub delivery_steps: Option<Arc<str>>,
    /// Input location (multi-step reception).
    #[field(many2one, comodel = "stock.location", string = "Input Location")]
    pub wh_input_stock_loc_id: Option<RecId>,
    /// Quality control location (three-step reception).
    #[field(
        many2one,
        comodel = "stock.location",
        string = "Quality Control Location"
    )]
    pub wh_qc_stock_loc_id: Option<RecId>,
    /// Output location (multi-step delivery).
    #[field(many2one, comodel = "stock.location", string = "Output Location")]
    pub wh_output_stock_loc_id: Option<RecId>,
    /// Packing location (three-step delivery).
    #[field(many2one, comodel = "stock.location", string = "Packing Location")]
    pub wh_pack_stock_loc_id: Option<RecId>,
    /// The warehouse's make-to-order rule.
    #[field(many2one, comodel = "stock.rule", copy = false, string = "MTO rule")]
    pub mto_pull_id: Option<RecId>,
    /// Pick operation type.
    #[field(
        many2one,
        comodel = "stock.picking.type",
        copy = false,
        string = "Pick Type"
    )]
    pub pick_type_id: Option<RecId>,
    /// Pack operation type.
    #[field(
        many2one,
        comodel = "stock.picking.type",
        copy = false,
        string = "Pack Type"
    )]
    pub pack_type_id: Option<RecId>,
    /// Delivery operation type.
    #[field(
        many2one,
        comodel = "stock.picking.type",
        copy = false,
        string = "Out Type"
    )]
    pub out_type_id: Option<RecId>,
    /// Receipt operation type.
    #[field(
        many2one,
        comodel = "stock.picking.type",
        copy = false,
        string = "In Type"
    )]
    pub in_type_id: Option<RecId>,
    /// Internal transfer operation type.
    #[field(
        many2one,
        comodel = "stock.picking.type",
        copy = false,
        string = "Internal Type"
    )]
    pub int_type_id: Option<RecId>,
    /// Quality control operation type.
    #[field(
        many2one,
        comodel = "stock.picking.type",
        copy = false,
        string = "Quality Control Type"
    )]
    pub qc_type_id: Option<RecId>,
    /// Storage operation type.
    #[field(
        many2one,
        comodel = "stock.picking.type",
        copy = false,
        string = "Storage Type"
    )]
    pub store_type_id: Option<RecId>,
    /// Cross-dock operation type.
    #[field(
        many2one,
        comodel = "stock.picking.type",
        copy = false,
        string = "Cross Dock Type"
    )]
    pub xdock_type_id: Option<RecId>,
    /// Receipt route.
    #[field(
        many2one,
        comodel = "stock.route",
        ondelete = "restrict",
        copy = false,
        string = "Receipt Route"
    )]
    pub reception_route_id: Option<RecId>,
    /// Delivery route.
    #[field(
        many2one,
        comodel = "stock.route",
        ondelete = "restrict",
        copy = false,
        string = "Delivery Route"
    )]
    pub delivery_route_id: Option<RecId>,
    /// Warehouses this one is resupplied from.
    #[field(
        many2many,
        comodel = "stock.warehouse",
        relation = "stock_wh_resupply_table",
        column1 = "supplied_wh_id",
        column2 = "supplier_wh_id",
        string = "Resupply From"
    )]
    pub resupply_wh_ids: Vec<RecId>,
    /// Resupply routes into this warehouse.
    #[field(
        one2many,
        comodel = "stock.route",
        inverse_name = "supplied_wh_id",
        copy = false,
        string = "Resupply Routes"
    )]
    pub resupply_route_ids: Vec<RecId>,
    /// Display order.
    #[field(integer, default = 10)]
    pub sequence: i32,
}

/// `stock.picking.type`: an operation type (receipt, delivery, internal transfer).
#[model(
    name = "stock.picking.type",
    description = "Picking Type",
    order = "sequence, id",
    rec_names_search = ["name", "warehouse_id.name"],
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    constraints = [
        (
            "_generated_role_complete",
            "CHECK ((generated_warehouse_id IS NULL) = (generated_addon IS NULL) AND (generated_addon IS NULL) = (generated_role IS NULL))",
            "A generated operation type names its warehouse, addon and role together."
        ),
        (
            "_generated_role_uniq",
            "UNIQUE (generated_warehouse_id, generated_addon, generated_role)",
            "A warehouse has one operation type per addon role."
        ),
    ],
)]
pub struct StockPickingType {
    /// Operation type name.
    #[field(char, required = true, translate = true, string = "Operation Type")]
    pub name: Option<Arc<str>>,
    /// Kanban color.
    #[field(integer)]
    pub color: i32,
    /// Display order.
    #[field(integer)]
    pub sequence: i32,
    /// Numbering of its transfers (a standard, gap-allowed sequence, topology ADR A1).
    #[field(
        many2one,
        comodel = "ir.sequence",
        copy = false,
        string = "Reference Sequence"
    )]
    pub sequence_id: Option<RecId>,
    /// Default source location of new transfers.
    #[field(
        many2one,
        comodel = "stock.location",
        required = true,
        string = "Source Location"
    )]
    pub default_location_src_id: Option<RecId>,
    /// Default destination location of new transfers.
    #[field(
        many2one,
        comodel = "stock.location",
        required = true,
        string = "Destination Location"
    )]
    pub default_location_dest_id: Option<RecId>,
    /// Where allocated products are sent.
    #[field(
        many2one,
        comodel = "stock.location",
        string = "Location for allocation"
    )]
    pub allocated_location_id: Option<RecId>,
    /// Kind of operation.
    #[field(
        selection,
        selection = [
            ("incoming", "Receipt"),
            ("outgoing", "Delivery"),
            ("internal", "Internal Transfer")
        ],
        default = "incoming",
        required = true,
        string = "Type of Operation"
    )]
    pub code: Option<Arc<str>>,
    /// Operation type used for returns.
    #[field(
        many2one,
        comodel = "stock.picking.type",
        index = "btree_not_null",
        string = "Operation Type for Returns"
    )]
    pub return_picking_type_id: Option<RecId>,
    /// Warehouse the type belongs to; deleting the warehouse deletes it.
    #[field(
        many2one,
        comodel = "stock.warehouse",
        ondelete = "cascade",
        index = true,
        string = "Warehouse"
    )]
    pub warehouse_id: Option<RecId>,
    /// Archived types are hidden.
    #[field(boolean, default = true)]
    pub active: bool,
    /// New lots or serials may be created in its transfers.
    #[field(boolean, default = true, string = "Create New Lots/Serial Numbers")]
    pub use_create_lots: bool,
    /// Existing lots or serials may be chosen in its transfers.
    #[field(boolean, default = true, string = "Use Existing Lots/Serial Numbers")]
    pub use_existing_lots: bool,
    /// Shipping labels are generated.
    #[field(boolean, default = false, string = "Generate Shipping Labels")]
    pub print_label: bool,
    /// When its transfers reserve stock.
    #[field(
        selection,
        selection = [
            ("at_confirm", "At Confirmation"),
            ("manual", "Manually"),
            ("by_date", "Before scheduled date")
        ],
        default = "at_confirm",
        required = true,
        string = "Reservation Method"
    )]
    pub reservation_method: Option<Arc<str>>,
    /// Days before the scheduled date to reserve (by date).
    #[field(integer, string = "Days")]
    pub reservation_days_before: i32,
    /// Days before the scheduled date to reserve a starred transfer (by date).
    #[field(integer, string = "Days when starred")]
    pub reservation_days_before_priority: i32,
    /// Barcode.
    #[field(char, copy = false)]
    pub barcode: Option<Arc<str>>,
    /// Owning company.
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        index = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Backorder policy on validation.
    #[field(
        selection,
        selection = [("ask", "Ask"), ("always", "Always"), ("never", "Never")],
        default = "ask",
        required = true,
        string = "Create Backorder"
    )]
    pub create_backorder: Option<Arc<str>>,
    /// Shipping policy; the company's policy as default comes with.
    #[field(
        selection,
        selection = [
            ("direct", "As soon as possible, with back orders"),
            ("one", "When all products are ready")
        ],
        default = "direct",
        required = true,
        string = "Shipping Policy"
    )]
    pub move_type: Option<Arc<str>>,
    /// Users who starred the type.
    #[field(
        many2many,
        comodel = "res.users",
        relation = "picking_type_favorite_user_rel",
        column1 = "picking_type_id",
        column2 = "user_id"
    )]
    pub favorite_user_ids: Vec<RecId>,
    /// Warehouse whose generated role this type fills (topology ADR D9).
    #[field(
        many2one,
        comodel = "stock.warehouse",
        ondelete = "restrict",
        copy = false,
        string = "Generated For Warehouse"
    )]
    pub generated_warehouse_id: Option<RecId>,
    /// Addon that owns the generated role.
    #[field(char, copy = false)]
    pub generated_addon: Option<Arc<str>>,
    /// Role name within the warehouse and addon.
    #[field(char, copy = false)]
    pub generated_role: Option<Arc<str>>,
}
