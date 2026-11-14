// SPDX-License-Identifier: MIT OR Apache-2.0
//! Warehouses and operation types with immutable company ownership. Generating a warehouse's
//! locations, types, routes and rules, the environment defaults and the step changes belong to;
//! numbering to (topology ADR A1).

use erp_sdk::prelude::*;

use crate::error::StockError;

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
            "Another warehouse of this company already has this name."
        ),
        (
            "_warehouse_code_uniq",
            "UNIQUE (code, company_id)",
            "Another warehouse of this company already has this short name."
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
            ("one_step", "Receive into stock (1 step)"),
            ("two_steps", "Receive at input, then put away (2 steps)"),
            ("three_steps", "Receive, check quality, put away (3 steps)")
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
            ("pick_ship", "Pick, then ship (2 steps)"),
            ("pick_pack_ship", "Pick, pack and ship (3 steps)")
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
        (
            "_reservation_days_bounds",
            "CHECK (reservation_days_before BETWEEN 0 AND 365 AND reservation_days_before_priority BETWEEN 0 AND 365)",
            "stock.picking.type reservation lead times must be between 0 and 365 days."
        ),
        (
            "_reservation_days_method",
            "CHECK (reservation_method = 'by_date' OR (reservation_days_before = 0 AND reservation_days_before_priority = 0))",
            "stock.picking.type reservation lead times must be zero unless reservation_method is by_date."
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
    /// Editable prefix of the linked sequence; copied types receive an independent sequence.
    #[field(
        char,
        related = "sequence_id.prefix",
        readonly = false,
        compute_sudo = false,
        copy = false,
        string = "Sequence Prefix"
    )]
    pub sequence_code: Option<Arc<str>>,
    /// Default source location of new transfers.
    #[field(
        many2one,
        comodel = "stock.location",
        required = true,
        compute = "_compute_default_locations",
        store = true,
        readonly = false,
        compute_sudo = false,
        check_company = true,
        string = "Source Location"
    )]
    pub default_location_src_id: Option<RecId>,
    /// Default destination location of new transfers.
    #[field(
        many2one,
        comodel = "stock.location",
        required = true,
        compute = "_compute_default_locations",
        store = true,
        readonly = false,
        compute_sudo = false,
        check_company = true,
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
    #[field(integer, default = 0, required = true, string = "Days")]
    pub reservation_days_before: i32,
    /// Days before the scheduled date to reserve a starred transfer (by date).
    #[field(integer, default = 0, required = true, string = "Days when starred")]
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
    /// Shipping policy; company defaults belong to.
    #[field(
        selection,
        selection = [
            ("direct", "Ship what is ready, back order the rest"),
            ("one", "Ship once everything is ready")
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
    /// Favorite membership of the current actor; the cache varies with the actor.
    #[field(
        boolean,
        compute = "_compute_is_favorite",
        inverse = "_inverse_is_favorite",
        search_method = "_search_is_favorite",
        copy = false
    )]
    pub is_favorite: bool,
    /// Transfers counted by the overview hook; this relation stores no extra column.
    #[field(
        one2many,
        comodel = "stock.picking",
        inverse_name = "picking_type_id",
        readonly = true,
        copy = false
    )]
    pub picking_ids: Vec<RecId>,
    /// Draft transfers visible to the reader.
    #[field(integer, compute = "_compute_picking_count")]
    pub count_picking_draft: i32,
    /// Confirmed and waiting transfers visible to the reader.
    #[field(integer, compute = "_compute_picking_count")]
    pub count_picking_waiting: i32,
    /// Assigned transfers visible to the reader.
    #[field(integer, compute = "_compute_picking_count")]
    pub count_picking_ready: i32,
    /// Assigned, waiting and confirmed transfers visible to the reader.
    #[field(integer, compute = "_compute_picking_count")]
    pub count_picking: i32,
    /// Active transfers past their date or with a deadline issue.
    #[field(integer, compute = "_compute_picking_count")]
    pub count_picking_late: i32,
    /// Active transfers created as backorders.
    #[field(integer, compute = "_compute_picking_count")]
    pub count_picking_backorders: i32,
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

// Use the public authorization API: base_check_write_fields is a reserved base-addon bridge.
// Do not read ownership until the caller may write every known field supplied in this call.
pub(crate) async fn check_company_write(
    records: &Records,
    tx: &mut Transaction,
    vals: &RawVals,
) -> Result<()> {
    records.check_access(tx, erp_sdk::Operation::Write).await?;
    let spec = records.spec();
    for name in vals
        .iter()
        .map(|(name, _)| name)
        .chain(vals.iter_commands().map(|(name, _)| name))
    {
        // Unknown names remain the base writer's validation responsibility; no permission is
        // inferred for them, and this helper never writes any supplied value.
        if spec.field(name).is_some()
            && !tx
                .has_field_access(records.env(), spec.name, name, FieldOp::Write)
                .await?
        {
            return Err(Error::access(format!(
                "Writing {}.{name} is not allowed.",
                spec.name
            )));
        }
    }
    let requested = match vals.get("company_id") {
        Some(Cell::Id(id)) if RecId::from_real(*id).is_some() => Some(*id),
        Some(Cell::Null) => None,
        Some(_) => {
            return Err(Error::validation(format!(
                "Invalid value for {}.company_id: expected a positive company id or null.",
                spec.name
            )));
        }
        None => {
            return Err(Error::internal(
                "company guard called without a company assignment",
            ));
        }
    };
    for current in records.cells(tx, "company_id").await? {
        match current {
            Cell::Id(id) if RecId::from_real(id).is_some() => {
                if requested != Some(id) {
                    return Err(StockError::CompanyChangeForbidden.into());
                }
            }
            _ => {
                return Err(Error::internal(format!(
                    "{} has no valid owning company",
                    spec.name
                )));
            }
        }
    }
    Ok(())
}

#[methods(StockWarehouse, addon = "stock")]
impl StockWarehouse {
    #[method(override)]
    async fn create(
        this: &RecordSet<StockWarehouse>,
        tx: &mut Transaction,
        vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let created = sup.call(tx, this.as_records(), (vals_list,)).await?;
        crate::res_company::invalidate_company_locations(tx).await?;
        Ok(created)
    }

    #[method(override)]
    async fn unlink(
        this: &RecordSet<StockWarehouse>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        sup.call(tx, this.as_records(), ()).await?;
        crate::res_company::invalidate_company_locations(tx).await
    }

    /// Ownership cannot change after creation, including on archived rows.
    /// Unchanged assignments preserve the base write and its company-link checks.
    #[method(override)]
    async fn write(
        this: &RecordSet<StockWarehouse>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        if !this.is_empty() && vals.contains("company_id") {
            check_company_write(this.as_records(), tx, &vals).await?;
        }
        sup.call(tx, this.as_records(), (vals,)).await?;
        crate::res_company::invalidate_company_locations(tx).await
    }
}

#[methods(StockPickingType, addon = "stock")]
impl StockPickingType {
    /// Ownership cannot change after creation, including on archived rows.
    /// Validate the whole batch before allowing any other supplied field to be written.
    #[method(override)]
    async fn write(
        this: &RecordSet<StockPickingType>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        crate::operation_type::write(this.as_records(), tx, vals, sup).await
    }
}
