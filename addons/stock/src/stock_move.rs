// SPDX-License-Identifier: MIT OR Apache-2.0
//! Stock moves (`stock.move`): table only. The
//! move units (confirm, reserve, done), the reference compute, the counter maintenance of the
//! transfer and the UoM reference registration of `uom_id` are's and
//! later slices'; nothing here writes them.

use erp_sdk::DefaultContext;
use erp_sdk::prelude::*;

fn now(_ctx: &DefaultContext) -> Value {
    Value::DateTime(DateTime::now())
}

/// `stock.move`: a demand to move a quantity of one product between two locations.
///
/// Product-unit quantities (6 with amendment C3): `product_qty` and
/// `quantity_product_uom` are stored, readonly and required with no default, next to the inputs
/// they were converted with (`uom_factor`, `product_uom_factor`, `product_unit_digits`). A generated
/// column cannot be declared for them ('s generated expressions have no division and no
/// `round`), so a table check binds each one to the unit service's SQL form of the conversion
/// (`erp_addon_uom::stock::product_quantity_sql`: `qty * uom_factor / product_uom_factor`, HALF-UP at
/// `product_unit_digits`). The finite, factor and digits checks are named so PostgreSQL, which tests
/// CHECKs in name order, reports a NaN, a zero factor or out-of-range digits before it evaluates a
/// conversion (which would otherwise raise a division error).
///
/// Company scope: its own company only; the
/// product declares its check (its company comes through the template delegation), the other
/// company-scoped links are checked by the framework default.
#[model(
    name = "stock.move",
    description = "Stock Move",
    order = "sequence, id",
    rec_name = "reference",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    constraints = [
        // the bound `decimal.precision` enforces on every digits setting
        // (`addons/base/src/decimal_precision.rs:34-38`, `_digits_range`), as the unit service does
        // (`addons/uom/src/stock.rs:54-56`)
        (
            "_digits_range",
            "CHECK (product_unit_digits >= 0 AND product_unit_digits <= 28)",
            "The Product Unit digits of a stock move must be between 0 and 28."
        ),
        (
            // a stored unit may have a negative factor, only zero is refused
            // (`addons/uom/src/models.rs:29-33`, `CHECK (relative_factor != 0)`)
            "_factor_nonzero",
            "CHECK (uom_factor <> 0 AND product_uom_factor <> 0)",
            "The unit factors of a stock move cannot be zero."
        ),
        (
            "_finite_product_uom_qty",
            "CHECK (product_uom_qty <> 'NaN' AND product_uom_qty <> 'Infinity' AND product_uom_qty <> '-Infinity')",
            "The demand of a stock move must be a finite number."
        ),
        (
            "_finite_quantity",
            "CHECK (quantity <> 'NaN' AND quantity <> 'Infinity' AND quantity <> '-Infinity')",
            "The quantity of a stock move must be a finite number."
        ),
        (
            "_finite_product_qty",
            "CHECK (product_qty <> 'NaN' AND product_qty <> 'Infinity' AND product_qty <> '-Infinity')",
            "The demand in the product unit of a stock move must be a finite number."
        ),
        (
            "_finite_quantity_product_uom",
            "CHECK (quantity_product_uom <> 'NaN' AND quantity_product_uom <> 'Infinity' AND quantity_product_uom <> '-Infinity')",
            "The quantity in the product unit of a stock move must be a finite number."
        ),
        (
            "_finite_uom_factor",
            "CHECK (uom_factor <> 'NaN' AND uom_factor <> 'Infinity' AND uom_factor <> '-Infinity')",
            "The unit factor of a stock move must be a finite number."
        ),
        (
            "_finite_product_uom_factor",
            "CHECK (product_uom_factor <> 'NaN' AND product_uom_factor <> 'Infinity' AND product_uom_factor <> '-Infinity')",
            "The product unit factor of a stock move must be a finite number."
        ),
        (
            "_finite_price_unit",
            "CHECK (price_unit <> 'NaN' AND price_unit <> 'Infinity' AND price_unit <> '-Infinity')",
            "The unit price of a stock move must be a finite number."
        ),
        (
            "_finite_packaging_uom_qty",
            "CHECK (packaging_uom_qty <> 'NaN' AND packaging_uom_qty <> 'Infinity' AND packaging_uom_qty <> '-Infinity')",
            "The packaging quantity of a stock move must be a finite number."
        ),
        (
            "_open_line_version_nonneg",
            "CHECK (open_line_version >= 0)",
            "The open line version of a stock move cannot be negative."
        ),
        // the exact text of `erp_addon_uom::stock::product_quantity_sql` for these columns (pinned by
        // the schema test): exact integer division with ties away from zero, so it never
        // double-rounds near a tie as `round(qty * factor / factor, digits)` does
        (
            "_product_qty_conversion",
            "CHECK (product_qty = round((div((\"product_uom_qty\" * \"uom_factor\" * (10::numeric ^ \"product_unit_digits\"::numeric)), \"product_uom_factor\") + CASE WHEN 2 * abs((\"product_uom_qty\" * \"uom_factor\" * (10::numeric ^ \"product_unit_digits\"::numeric)) - div((\"product_uom_qty\" * \"uom_factor\" * (10::numeric ^ \"product_unit_digits\"::numeric)), \"product_uom_factor\") * \"product_uom_factor\") >= abs(\"product_uom_factor\") THEN sign((\"product_uom_qty\" * \"uom_factor\" * (10::numeric ^ \"product_unit_digits\"::numeric))) * sign(\"product_uom_factor\") ELSE 0 END) * (10::numeric ^ (-\"product_unit_digits\"::numeric)), \"product_unit_digits\"))",
            "The demand in the product unit must be the demand converted with the stored unit factors and digits."
        ),
        (
            "_quantity_product_uom_conversion",
            "CHECK (quantity_product_uom = round((div((\"quantity\" * \"uom_factor\" * (10::numeric ^ \"product_unit_digits\"::numeric)), \"product_uom_factor\") + CASE WHEN 2 * abs((\"quantity\" * \"uom_factor\" * (10::numeric ^ \"product_unit_digits\"::numeric)) - div((\"quantity\" * \"uom_factor\" * (10::numeric ^ \"product_unit_digits\"::numeric)), \"product_uom_factor\") * \"product_uom_factor\") >= abs(\"product_uom_factor\") THEN sign((\"quantity\" * \"uom_factor\" * (10::numeric ^ \"product_unit_digits\"::numeric))) * sign(\"product_uom_factor\") ELSE 0 END) * (10::numeric ^ (-\"product_unit_digits\"::numeric)), \"product_unit_digits\"))",
            "The quantity in the product unit must be the quantity converted with the stored unit factors and digits."
        ),
        // target of the move line's composite key `stock_move_line_move_company_product` (
        // D2.2 I1); `stock_move.rs` sorts before `stock_move_line.rs`, so it exists first
        (
            "_id_company_product",
            "UNIQUE (id, company_id, product_id)",
            "A stock move is unique with its company and product."
        ),
    ],
    indexes = [index(
        name = "_open_product_location",
        fields = ["product_id", "location_id", "location_dest_id", "company_id"],
        filter = "[('state', 'not in', ['done', 'cancel'])]"
    )],
)]
pub struct StockMove {
    /// Display order within the transfer.
    #[field(integer, default = 10, string = "Sequence")]
    pub sequence: i32,
    /// Reservation priority; computed from the transfer.
    #[field(
        selection,
        selection = [("0", "Normal"), ("1", "Urgent")],
        default = "0",
        string = "Priority"
    )]
    pub priority: Option<Arc<str>>,
    /// Scheduled date until the move is done, then the date it was processed.
    #[field(
        datetime,
        required = true,
        index = true,
        default_fn = now,
        string = "Date Scheduled"
    )]
    pub date: Option<DateTime>,
    /// Deadline.
    #[field(datetime, readonly = true, copy = false, string = "Deadline")]
    pub date_deadline: Option<DateTime>,
    /// Company.
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        ondelete = "restrict",
        index = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Product.
    #[field(
        many2one,
        comodel = "product.product",
        required = true,
        ondelete = "restrict",
        index = true,
        check_company = true,
        string = "Product"
    )]
    pub product_id: Option<RecId>,
    /// Attribute values the product never has.
    #[field(
        many2many,
        comodel = "product.template.attribute.value",
        relation = "template_attribute_value_stock_move_rel",
        column1 = "move_id",
        column2 = "template_attribute_value_id",
        string = "Never attribute Values"
    )]
    pub never_product_template_attribute_value_ids: Vec<RecId>,
    /// Picking description written by hand.
    #[field(text, readonly = true)]
    pub description_picking_manual: Option<Arc<str>>,
    /// Demand in the product unit: the demand converted with the stored factors and digits.
    #[field(
        float,
        digits = 0,
        required = true,
        readonly = true,
        string = "Demand in Product UoM"
    )]
    pub product_qty: Decimal,
    /// Demand in the move's unit.
    #[field(
        float,
        digits = "Product Unit",
        required = true,
        default = 0,
        string = "Demand"
    )]
    pub product_uom_qty: Decimal,
    /// The move's unit.
    #[field(
        many2one,
        comodel = "uom.uom",
        required = true,
        ondelete = "restrict",
        index = true,
        string = "Unit"
    )]
    pub uom_id: Option<RecId>,
    /// Absolute factor of the move's unit the product-unit quantities were converted with.
    #[field(
        float,
        digits = 0,
        required = true,
        readonly = true,
        string = "Unit Factor"
    )]
    pub uom_factor: Decimal,
    /// Absolute factor of the product's unit the product-unit quantities were converted with.
    #[field(
        float,
        digits = 0,
        required = true,
        readonly = true,
        string = "Product Unit Factor"
    )]
    pub product_uom_factor: Decimal,
    /// Product Unit digits in force when the product-unit quantities were converted.
    #[field(
        integer,
        required = true,
        readonly = true,
        string = "Product Unit Digits"
    )]
    pub product_unit_digits: i32,
    /// Source location.
    #[field(
        many2one,
        comodel = "stock.location",
        required = true,
        ondelete = "restrict",
        index = true,
        string = "Source Location"
    )]
    pub location_id: Option<RecId>,
    /// Destination location.
    #[field(
        many2one,
        comodel = "stock.location",
        required = true,
        ondelete = "restrict",
        index = true,
        string = "Intermediate Location"
    )]
    pub location_dest_id: Option<RecId>,
    /// Location used by the forecast.
    #[field(
        many2one,
        comodel = "stock.location",
        index = true,
        string = "Forecasted Location"
    )]
    pub forecasted_location_id: Option<RecId>,
    /// Delivery address.
    #[field(
        many2one,
        comodel = "res.partner",
        index = "btree_not_null",
        string = "Destination Address"
    )]
    pub partner_id: Option<RecId>,
    /// Next moves of the chain (`stock_move_move_rel`, origin to destination).
    #[field(
        many2many,
        comodel = "stock.move",
        relation = "stock_move_move_rel",
        column1 = "move_orig_id",
        column2 = "move_dest_id",
        copy = false,
        string = "Destination Moves"
    )]
    pub move_dest_ids: Vec<RecId>,
    /// Previous moves of the chain (the same table, read the other way).
    #[field(
        many2many,
        comodel = "stock.move",
        relation = "stock_move_move_rel",
        column1 = "move_dest_id",
        column2 = "move_orig_id",
        copy = false,
        string = "Original Move"
    )]
    pub move_orig_ids: Vec<RecId>,
    /// Transfer.
    #[field(
        many2one,
        comodel = "stock.picking",
        ondelete = "restrict",
        index = true,
        string = "Transfer"
    )]
    pub picking_id: Option<RecId>,
    /// Status, written by the move units.
    #[field(
        selection,
        selection = [
            ("draft", "New"),
            ("waiting", "Waiting Another Move"),
            ("confirmed", "Waiting"),
            ("partially_available", "Partially Available"),
            ("assigned", "Available"),
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
    /// Picked (indicative only).
    #[field(boolean, default = false, copy = false, string = "Picked")]
    pub picked: bool,
    /// Unit cost recorded at confirmation, company currency, product unit.
    #[field(float, digits = 0, copy = false, string = "Unit Price")]
    pub price_unit: Decimal,
    /// Source document.
    #[field(char, string = "Source Document")]
    pub origin: Option<Arc<str>>,
    /// Supply method.
    #[field(
        selection,
        selection = [
            ("make_to_stock", "Default: Take From Stock"),
            ("make_to_order", "Advanced: Apply Procurement Rules")
        ],
        required = true,
        copy = false,
        default = "make_to_stock",
        string = "Supply Method"
    )]
    pub procure_method: Option<Arc<str>>,
    /// Scrap move.
    #[field(boolean, readonly = true)]
    pub is_scrap: bool,
    /// References of the move (the other side of `stock_reference_move_rel`).
    #[field(
        many2many,
        comodel = "stock.reference",
        relation = "stock_reference_move_rel",
        column1 = "move_id",
        column2 = "reference_id",
        string = "References"
    )]
    pub reference_ids: Vec<RecId>,
    /// Rule that created the move.
    #[field(
        many2one,
        comodel = "stock.rule",
        ondelete = "restrict",
        index = "btree_not_null",
        string = "Stock Rule"
    )]
    pub rule_id: Option<RecId>,
    /// Cancelling or splitting this move does the same to the next one.
    #[field(boolean, default = true, string = "Propagate cancel and split")]
    pub propagate_cancel: bool,
    /// Operation type.
    #[field(
        many2one,
        comodel = "stock.picking.type",
        index = "btree_not_null",
        string = "Operation Type"
    )]
    pub picking_type_id: Option<RecId>,
    /// Inventory adjustment move.
    #[field(boolean, string = "Inventory")]
    pub is_inventory: bool,
    /// Name of the inventory adjustment.
    #[field(char, readonly = true)]
    pub inventory_name: Option<Arc<str>>,
    /// Move this return was created from.
    #[field(
        many2one,
        comodel = "stock.move",
        copy = false,
        index = true,
        string = "Origin return move"
    )]
    pub origin_returned_move_id: Option<RecId>,
    /// Owner the quants must belong to when the move is done.
    #[field(
        many2one,
        comodel = "res.partner",
        index = "btree_not_null",
        string = "Owner"
    )]
    pub restrict_partner_id: Option<RecId>,
    /// Preferred routes of the next procurement.
    #[field(
        many2many,
        comodel = "stock.route",
        relation = "stock_route_move",
        column1 = "move_id",
        column2 = "route_id",
        string = "Destination route"
    )]
    pub route_ids: Vec<RecId>,
    /// Warehouse considered for the route of the next procurement.
    #[field(
        many2one,
        comodel = "stock.warehouse",
        index = "btree_not_null",
        string = "Warehouse"
    )]
    pub warehouse_id: Option<RecId>,
    /// Quantity in the move's unit, written by the line statements (2 I3).
    #[field(
        float,
        digits = "Product Unit",
        required = true,
        readonly = true,
        default = 0,
        string = "Quantity"
    )]
    pub quantity: Decimal,
    /// Quantity in the product unit: the quantity converted with the stored factors and digits.
    #[field(
        float,
        digits = 0,
        required = true,
        readonly = true,
        copy = false,
        string = "Quantity in Product UoM"
    )]
    pub quantity_product_uom: Decimal,
    /// Added after the transfer's confirmation.
    #[field(
        boolean,
        default = false,
        string = "Whether the move was added after the picking's confirmation"
    )]
    pub additional: bool,
    /// Reference; its compute is's.
    #[field(char, readonly = true, copy = false, string = "Reference")]
    pub reference: Option<Arc<str>>,
    /// First serial or lot number to generate.
    #[field(char, string = "First SN/Lot")]
    pub next_serial: Option<Arc<str>>,
    /// Number of serial or lot numbers to generate.
    #[field(integer, string = "Number of SN/Lots")]
    pub next_serial_count: i32,
    /// Date the move should be reserved.
    #[field(date, string = "Date to Reserve")]
    pub reservation_date: Option<Date>,
    /// Packaging unit from the sale or purchase order.
    #[field(
        many2one,
        comodel = "uom.uom",
        index = "btree_not_null",
        string = "Packaging"
    )]
    pub packaging_uom_id: Option<RecId>,
    /// Quantity in the packaging unit.
    #[field(float, digits = "Product Unit", string = "Packaging Quantity")]
    pub packaging_uom_qty: Decimal,
    /// Replenish the scrapped quantity.
    #[field(boolean, string = "Should Replenish")]
    pub should_replenish_scrapped: bool,
    /// Bumped by every change to the move's open lines (1, D2.3).
    #[field(integer, required = true, readonly = true, default = 0)]
    pub open_line_version: i32,
    /// When the move was done (1); never changed afterwards (its trigger is
    /// 's, amendment C1).
    #[field(datetime, readonly = true, copy = false, string = "Done At")]
    pub done_at: Option<DateTime>,
}
