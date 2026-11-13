// SPDX-License-Identifier: MIT OR Apache-2.0
//! Routes, rules and the topology version row: tables only. Validation, the
//! archive cascade, route resolution, the generation bump and the topology cache belong to
//! (topology ADR D1, D2, D10).

use erp_sdk::prelude::*;

/// `stock.route`: a set of rules a product, category, package type or warehouse can follow.
#[model(
    name = "stock.route",
    description = "Inventory Routes",
    order = "sequence",
    company_access = direct(field = "company_id", branch = "exact", empty = "allow"),
    constraints = [
        (
            "_generated_role_complete",
            "CHECK ((generated_warehouse_id IS NULL) = (generated_addon IS NULL) AND (generated_addon IS NULL) = (generated_role IS NULL))",
            "A generated route names its warehouse, addon and role together."
        ),
        (
            "_generated_role_uniq",
            "UNIQUE (generated_warehouse_id, generated_addon, generated_role)",
            "A warehouse has one route per addon role."
        ),
    ],
)]
pub struct StockRoute {
    /// Route name.
    #[field(char, required = true, translate = true, string = "Route")]
    pub name: Option<Arc<str>>,
    /// Archived routes are not applied.
    #[field(boolean, default = true)]
    pub active: bool,
    /// Priority among routes (lower first).
    #[field(integer, default = 0)]
    pub sequence: i32,
    /// The rules of the route.
    #[field(
        one2many,
        comodel = "stock.rule",
        inverse_name = "route_id",
        string = "Rules"
    )]
    pub rule_ids: Vec<RecId>,
    /// Selectable on products.
    #[field(boolean, default = true, string = "Applicable on Product")]
    pub product_selectable: bool,
    /// Selectable on product categories.
    #[field(boolean, default = false, string = "Applicable on Product Category")]
    pub product_categ_selectable: bool,
    /// Selectable on warehouses.
    #[field(boolean, default = false, string = "Applicable on Warehouse")]
    pub warehouse_selectable: bool,
    /// Selectable on package types.
    #[field(boolean, default = false, string = "Applicable on Package Type")]
    pub package_type_selectable: bool,
    /// Warehouse supplied by this resupply route.
    #[field(
        many2one,
        comodel = "stock.warehouse",
        index = "btree_not_null",
        string = "Supplied Warehouse"
    )]
    pub supplied_wh_id: Option<RecId>,
    /// Warehouse supplying through this resupply route.
    #[field(
        many2one,
        comodel = "stock.warehouse",
        index = "btree_not_null",
        string = "Supplying Warehouse"
    )]
    pub supplier_wh_id: Option<RecId>,
    /// Owning company; empty for a route shared between companies.
    #[field(many2one, comodel = "res.company", index = true, string = "Company")]
    pub company_id: Option<RecId>,
    /// Products following this route.
    #[field(
        many2many,
        comodel = "product.template",
        relation = "stock_route_product",
        column1 = "route_id",
        column2 = "product_id",
        copy = false,
        string = "Products"
    )]
    pub product_ids: Vec<RecId>,
    /// Product categories following this route.
    #[field(
        many2many,
        comodel = "product.category",
        relation = "stock_route_categ",
        column1 = "route_id",
        column2 = "categ_id",
        copy = false,
        string = "Product Categories"
    )]
    pub categ_ids: Vec<RecId>,
    /// Warehouses following this route.
    #[field(
        many2many,
        comodel = "stock.warehouse",
        relation = "stock_route_warehouse",
        column1 = "route_id",
        column2 = "warehouse_id",
        copy = false,
        string = "Warehouses"
    )]
    pub warehouse_ids: Vec<RecId>,
    /// Package types following this route.
    #[field(
        many2many,
        comodel = "stock.package.type",
        relation = "stock_package_type_stock_route_rel",
        column1 = "stock_route_id",
        column2 = "stock_package_type_id",
        copy = false,
        string = "Package Types"
    )]
    pub package_type_ids: Vec<RecId>,
    /// Warehouse whose generated role this route fills (topology ADR D9).
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

/// `stock.rule`: one pull or push step of a route.
#[model(
    name = "stock.rule",
    description = "Stock Rule",
    order = "sequence, id",
    company_access = direct(field = "company_id", branch = "exact", empty = "allow"),
    constraints = [
        (
            "_generated_role_complete",
            "CHECK ((generated_warehouse_id IS NULL) = (generated_addon IS NULL) AND (generated_addon IS NULL) = (generated_role IS NULL))",
            "A generated rule names its warehouse, addon and role together."
        ),
        (
            "_generated_role_uniq",
            "UNIQUE (generated_warehouse_id, generated_addon, generated_role)",
            "A warehouse has one rule per addon role."
        ),
    ],
)]
pub struct StockRule {
    /// Rule name; fills the origin of the transfers it creates.
    #[field(char, required = true, translate = true)]
    pub name: Option<Arc<str>>,
    /// Archived rules are not applied.
    #[field(boolean, default = true)]
    pub active: bool,
    /// What the rule does.
    #[field(
        selection,
        selection = [("pull", "Pull From"), ("push", "Push To"), ("pull_push", "Pull & Push")],
        default = "pull",
        required = true,
        index = true
    )]
    pub action: Option<Arc<str>>,
    /// Order within the route.
    #[field(integer, default = 20)]
    pub sequence: i32,
    /// Owning company; empty for a shared rule.
    #[field(many2one, comodel = "res.company", index = true, string = "Company")]
    pub company_id: Option<RecId>,
    /// Destination location.
    #[field(
        many2one,
        comodel = "stock.location",
        required = true,
        index = true,
        string = "Destination Location"
    )]
    pub location_dest_id: Option<RecId>,
    /// Source location.
    #[field(
        many2one,
        comodel = "stock.location",
        index = true,
        string = "Source Location"
    )]
    pub location_src_id: Option<RecId>,
    /// The move's destination comes from the rule instead of the operation type.
    #[field(
        boolean,
        default = false,
        string = "Destination location origin from rule"
    )]
    pub location_dest_from_rule: bool,
    /// Route of the rule; deleting the route deletes it.
    #[field(
        many2one,
        comodel = "stock.route",
        required = true,
        ondelete = "cascade",
        index = true,
        string = "Route"
    )]
    pub route_id: Option<RecId>,
    /// The route's priority, stored for ordering candidate rules.
    #[field(
        integer,
        related = "route_id.sequence",
        store = true,
        string = "Route Sequence"
    )]
    pub route_sequence: i32,
    /// Supply method.
    #[field(
        selection,
        selection = [
            ("make_to_stock", "Take From Stock"),
            ("make_to_order", "Trigger Another Rule"),
            ("mts_else_mto", "Take From Stock, if unavailable, Trigger Another Rule")
        ],
        default = "make_to_stock",
        required = true,
        string = "Supply Method"
    )]
    pub procure_method: Option<Arc<str>>,
    /// Operation type of the created transfers.
    #[field(
        many2one,
        comodel = "stock.picking.type",
        required = true,
        string = "Operation Type"
    )]
    pub picking_type_id: Option<RecId>,
    /// Lead time in days.
    #[field(integer, default = 0, string = "Lead Time")]
    pub delay: i32,
    /// Partner address of the created transfers.
    #[field(many2one, comodel = "res.partner", string = "Partner Address")]
    pub partner_address_id: Option<RecId>,
    /// Cancelling the created move cancels the next one.
    #[field(boolean, default = false, string = "Cancel Next Move")]
    pub propagate_cancel: bool,
    /// The carrier is carried to the next move.
    #[field(boolean, default = false, string = "Propagation of carrier")]
    pub propagate_carrier: bool,
    /// Warehouse the rule applies to.
    #[field(
        many2one,
        comodel = "stock.warehouse",
        index = true,
        string = "Warehouse"
    )]
    pub warehouse_id: Option<RecId>,
    /// A push either adds a step or only changes the destination.
    #[field(
        selection,
        selection = [("manual", "Manual Operation"), ("transparent", "Automatic No Step Added")],
        default = "manual",
        required = true,
        string = "Automatic Move"
    )]
    pub auto: Option<Arc<str>>,
    /// Push applicability, a domain on moves stored as text.
    #[field(char, string = "Push Applicability")]
    pub push_domain: Option<Arc<str>>,
    /// Warehouse whose generated role this rule fills (topology ADR D9).
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

/// `stock.topology.version`: the one row per database whose generation every topology mutation
/// raises in its own transaction (topology ADR D1, A4). The install creates it; its absence is a
/// defect, never generation zero. The bump and the cache keyed by it come with.
#[model(
    name = "stock.topology.version",
    description = "Stock Topology Version",
    constraints = [
        (
            "_singleton_true",
            "CHECK (singleton IS TRUE)",
            "The stock topology version is a single row."
        ),
        (
            "_singleton_uniq",
            "UNIQUE (singleton)",
            "The stock topology version is a single row."
        ),
        (
            "_generation_nonneg",
            "CHECK (generation >= 0)",
            "The stock topology generation cannot be negative."
        ),
    ],
)]
pub struct StockTopologyVersion {
    /// Always true; with the unique constraint it allows one row only.
    #[field(boolean, default = true, required = true)]
    pub singleton: bool,
    /// Generation of the committed topology.
    #[field(integer, default = 0, required = true)]
    pub generation: i32,
}
