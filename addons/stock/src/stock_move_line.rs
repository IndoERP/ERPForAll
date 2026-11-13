// SPDX-License-Identifier: MIT OR Apache-2.0
//! Move lines (`stock.move.line`), their serial child rows (`stock.move.line.serial`) and their
//! correction rows (`stock.move.line.correction`): tables only (
//! D2.5, D4). The statements that write open
//! lines, children and corrections, the logical-row surface (8, amendment C2) and the
//! `done_at` and append-only triggers (amendment C1) are later slices'.
//!
//! The three models live in one file in this order on purpose: schema sync adds table constraints in
//! registry order (source file, then line), and each composite foreign key needs its `UNIQUE (id, …)`
//! target first (`stock_move_id_company_product` in `stock_move.rs`, the lot's in `lot.rs`, then the
//! line's, then the child's).

use erp_sdk::DefaultContext;
use erp_sdk::prelude::*;

/// The id sequence lines and serial children share (3): one integer id names a
/// line or a child, never both.
pub const LOGICAL_ID_SEQUENCE: &str = "stock_move_line_logical_id_seq";

fn now(_ctx: &DefaultContext) -> Value {
    Value::DateTime(DateTime::now())
}

/// `stock.move.line`: one operation line, the movement of one key group of a move (1).
///
/// The reservation index
/// holds open, active lines (D2.5). A child-mode line keeps no lot: its serial children carry them (D1.2).
/// No `state` or `date` copy of the move is stored (D2.5).
///
/// Composite keys hold every writer, ORM or SQL, to invariant I1's company and product
/// (`_move_company_product` onto the move) and to a lot of the line's product (`_lot_product`). The
/// product-unit copy is stored with its conversion inputs and bound to them by the unit service's SQL
/// form, as on [`crate::stock_move::StockMove`] (C3).
#[model(
    name = "stock.move.line",
    description = "Product Moves (Stock Move Line)",
    order = "result_package_id desc, id",
    rec_name = "product_id",
    id_sequence = LOGICAL_ID_SEQUENCE,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    constraints = [
        (
            "_quantity_nonneg",
            "CHECK (quantity >= 0)",
            "You can not enter negative quantities."
        ),
        (
            "_digits_range",
            "CHECK (product_unit_digits >= 0 AND product_unit_digits <= 28)",
            "The Product Unit digits of a move line must be between 0 and 28."
        ),
        (
            "_factor_nonzero",
            "CHECK (uom_factor <> 0 AND product_uom_factor <> 0)",
            "The unit factors of a move line cannot be zero."
        ),
        (
            "_finite_quantity",
            "CHECK (quantity <> 'NaN' AND quantity <> 'Infinity' AND quantity <> '-Infinity')",
            "The quantity of a move line must be a finite number."
        ),
        (
            "_finite_quantity_product_uom",
            "CHECK (quantity_product_uom <> 'NaN' AND quantity_product_uom <> 'Infinity' AND quantity_product_uom <> '-Infinity')",
            "The quantity in the product unit of a move line must be a finite number."
        ),
        (
            "_finite_uom_factor",
            "CHECK (uom_factor <> 'NaN' AND uom_factor <> 'Infinity' AND uom_factor <> '-Infinity')",
            "The unit factor of a move line must be a finite number."
        ),
        (
            "_finite_product_uom_factor",
            "CHECK (product_uom_factor <> 'NaN' AND product_uom_factor <> 'Infinity' AND product_uom_factor <> '-Infinity')",
            "The product unit factor of a move line must be a finite number."
        ),
        (
            "_correction_seq_nonneg",
            "CHECK (correction_seq >= 0)",
            "The correction counter of a move line cannot be negative."
        ),
        // a done line is in the ledger and has its first completion time (2 I4)
        (
            "_done_has_done_at",
            "CHECK (membership = 'open' OR done_at IS NOT NULL)",
            "A done move line must have its done time."
        ),
        // only a reopened row is deactivated, at quantity zero, and it is in neither ledger nor
        // reservation (11, D2.2 I4)
        (
            "_inactive_reopened",
            "CHECK (active OR (membership = 'open' AND done_at IS NOT NULL AND quantity = 0))",
            "Only a reopened move line with quantity zero can be deactivated."
        ),
        // the children of a child-mode line carry its lots (1, D1.2)
        (
            "_child_mode_no_lot",
            "CHECK (NOT child_mode OR (lot_id IS NULL AND lot_name IS NULL))",
            "A move line with serial rows keeps its lots on those rows."
        ),
        // the exact text of `erp_addon_uom::stock::product_quantity_sql` for these columns, the same as
        // the move's (pinned by the schema test)
        (
            "_quantity_product_uom_conversion",
            "CHECK (quantity_product_uom = round((div((\"quantity\" * \"uom_factor\" * (10::numeric ^ \"product_unit_digits\"::numeric)), \"product_uom_factor\") + CASE WHEN 2 * abs((\"quantity\" * \"uom_factor\" * (10::numeric ^ \"product_unit_digits\"::numeric)) - div((\"quantity\" * \"uom_factor\" * (10::numeric ^ \"product_unit_digits\"::numeric)), \"product_uom_factor\") * \"product_uom_factor\") >= abs(\"product_uom_factor\") THEN sign((\"quantity\" * \"uom_factor\" * (10::numeric ^ \"product_unit_digits\"::numeric))) * sign(\"product_uom_factor\") ELSE 0 END) * (10::numeric ^ (-\"product_unit_digits\"::numeric)), \"product_unit_digits\"))",
            "The quantity in the product unit must be the quantity converted with the stored unit factors and digits."
        ),
        // targets of the serial child's and the correction's composite keys
        (
            "_id_move",
            "UNIQUE (id, move_id)",
            "A move line is unique with its move."
        ),
        (
            "_id_company_product",
            "UNIQUE (id, company_id, product_id)",
            "A move line is unique with its company and product."
        ),
        // raw composite keys until slice B6 declares them
        (
            "_lot_product",
            "FOREIGN KEY (lot_id, product_id) REFERENCES stock_lot (id, product_id)",
            "The lot or serial number of a move line belongs to another product."
        ),
        (
            "_move_company_product",
            "FOREIGN KEY (move_id, company_id, product_id) REFERENCES stock_move (id, company_id, product_id)",
            "A move line has the company and the product of its move."
        ),
    ],
    unique_indexes = [(
        "_open_key",
        "(move_id, product_id, location_id, location_dest_id, lot_id, lot_name, package_id, result_package_id, owner_id, uom_id, picked) NULLS NOT DISTINCT WHERE membership = 'open' AND done_at IS NULL",
        "An open move line with the same move, product, locations, lot, packages, owner, unit and picked flag already exists."
    )],
    // the reservation truth (5,.1); benefit unmeasured (DEFERRED_PERFORMANCE)
    indexes = [(
        "_reservation",
        "(company_id, product_id, location_id, lot_id, package_id, owner_id) WHERE membership = 'open' AND active"
    )],
)]
pub struct StockMoveLine {
    /// Move of the line; every line has one (1).
    #[field(
        many2one,
        comodel = "stock.move",
        required = true,
        ondelete = "restrict",
        index = true,
        string = "Stock Operation"
    )]
    pub move_id: Option<RecId>,
    /// Transfer, the move's (2 I1, kept).
    #[field(
        many2one,
        comodel = "stock.picking",
        ondelete = "restrict",
        index = true,
        string = "Transfer"
    )]
    pub picking_id: Option<RecId>,
    /// Company, the move's.
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        readonly = true,
        ondelete = "restrict",
        index = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Product, the move's.
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
    /// Unit of the line's quantity.
    #[field(
        many2one,
        comodel = "uom.uom",
        required = true,
        ondelete = "restrict",
        index = true,
        string = "Unit"
    )]
    pub uom_id: Option<RecId>,
    /// Quantity in the line's unit; for a child-mode line, the number of its active serial rows.
    #[field(
        float,
        digits = "Product Unit",
        required = true,
        default = 0,
        copy = false,
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
    /// Absolute factor of the line's unit the product-unit quantity was converted with.
    #[field(
        float,
        digits = 0,
        required = true,
        readonly = true,
        string = "Unit Factor"
    )]
    pub uom_factor: Decimal,
    /// Absolute factor of the product's unit the product-unit quantity was converted with.
    #[field(
        float,
        digits = 0,
        required = true,
        readonly = true,
        string = "Product Unit Factor"
    )]
    pub product_uom_factor: Decimal,
    /// Product Unit digits in force when the product-unit quantity was converted.
    #[field(
        integer,
        required = true,
        readonly = true,
        string = "Product Unit Digits"
    )]
    pub product_unit_digits: i32,
    /// Picked; a key part, so required (a NULL would be a third key value).
    #[field(
        boolean,
        required = true,
        default = false,
        copy = false,
        string = "Picked"
    )]
    pub picked: bool,
    /// Package the stock is taken from.
    #[field(
        many2one,
        comodel = "stock.package",
        ondelete = "restrict",
        index = "btree_not_null",
        string = "Source Package"
    )]
    pub package_id: Option<RecId>,
    /// Lot or serial number of a plain line.
    #[field(
        many2one,
        comodel = "stock.lot",
        ondelete = "restrict",
        index = "btree_not_null",
        string = "Lot/Serial Number"
    )]
    pub lot_id: Option<RecId>,
    /// Name of a lot to create at done.
    #[field(char, string = "Lot/Serial Number Name")]
    pub lot_name: Option<Arc<str>>,
    /// Package the stock is put in.
    #[field(
        many2one,
        comodel = "stock.package",
        ondelete = "restrict",
        index = "btree_not_null",
        string = "Destination Package"
    )]
    pub result_package_id: Option<RecId>,
    /// Owner the stock is taken from. `restrict`: a key part.
    #[field(
        many2one,
        comodel = "res.partner",
        ondelete = "restrict",
        index = "btree_not_null",
        string = "From Owner"
    )]
    pub owner_id: Option<RecId>,
    /// Source location.
    #[field(
        many2one,
        comodel = "stock.location",
        required = true,
        ondelete = "restrict",
        index = true,
        string = "From"
    )]
    pub location_id: Option<RecId>,
    /// Destination location.
    #[field(
        many2one,
        comodel = "stock.location",
        required = true,
        ondelete = "restrict",
        index = true,
        string = "To"
    )]
    pub location_dest_id: Option<RecId>,
    /// Added through an entire package.
    #[field(boolean, string = "Is added through entire package")]
    pub is_entire_pack: bool,
    /// Open membership (5): `open` is the reservation, `done` the ledger. Written only by
    /// the transition statements.
    #[field(
        selection,
        selection = [("open", "Open"), ("done", "Done")],
        required = true,
        readonly = true,
        copy = false,
        default = "open",
        string = "Membership"
    )]
    pub membership: Option<Arc<str>>,
    /// False only for a deactivated reopened row (11).
    #[field(boolean, required = true, default = true, string = "Active")]
    pub active: bool,
    /// First completion time (1); never changed afterwards (its trigger is
    /// 's, amendment C1).
    #[field(datetime, readonly = true, copy = false, string = "Done At")]
    pub done_at: Option<DateTime>,
    /// Highest correction `seq` of the line (1, D2.2 I6).
    #[field(
        integer,
        required = true,
        readonly = true,
        default = 0,
        copy = false,
        string = "Correction Counter"
    )]
    pub correction_seq: i32,
    /// Serial units are child rows (2); fixed when the line is created.
    #[field(
        boolean,
        required = true,
        readonly = true,
        default = false,
        string = "Serial Rows"
    )]
    pub child_mode: bool,
}

/// `stock.move.line.serial`: one serial unit of a child-mode line (2, D1.3).
///
/// The id comes from the lines' sequence. The child's move is its line's (`_line_move`, re-pointed with
/// the line). A lot, or a lot name not yet resolved, is held once among a move's active children. The
/// key overrides are null while the child follows its line.
#[model(
    name = "stock.move.line.serial",
    description = "Move Line Serial",
    order = "line_id, id",
    log_access = false,
    id_sequence = LOGICAL_ID_SEQUENCE,
    company_access = parent(edge = "line_id"),
    constraints = [
        (
            "_line_move",
            "FOREIGN KEY (line_id, move_id) REFERENCES stock_move_line (id, move_id) ON UPDATE CASCADE ON DELETE CASCADE",
            "A serial row has the move of its line."
        ),
        // target of the correction's child key
        (
            "_id_line",
            "UNIQUE (id, line_id)",
            "A serial row is unique with its line."
        ),
    ],
    unique_indexes = [
        (
            "_move_lot",
            "(move_id, lot_id) WHERE active AND lot_id IS NOT NULL",
            "This serial number is already on another serial row of the move."
        ),
        (
            "_move_lot_name",
            "(move_id, lot_name) WHERE active AND lot_id IS NULL AND lot_name IS NOT NULL",
            "This serial number name is already on another serial row of the move."
        ),
    ],
)]
pub struct StockMoveLineSerial {
    /// The line; its open children go with it.
    #[field(
        many2one,
        comodel = "stock.move.line",
        required = true,
        ondelete = "cascade",
        index = true,
        string = "Move Line"
    )]
    pub line_id: Option<RecId>,
    /// The line's move (2 I1).
    #[field(
        many2one,
        comodel = "stock.move",
        required = true,
        ondelete = "restrict",
        index = true,
        string = "Stock Operation"
    )]
    pub move_id: Option<RecId>,
    /// Serial number; empty while the unit is not identified or only named.
    #[field(
        many2one,
        comodel = "stock.lot",
        ondelete = "restrict",
        index = "btree_not_null",
        string = "Lot/Serial Number"
    )]
    pub lot_id: Option<RecId>,
    /// Name of a serial to create at done.
    #[field(char, string = "Lot/Serial Number Name")]
    pub lot_name: Option<Arc<str>>,
    /// False once the unit is removed from a line with done history (11, D4.5).
    #[field(boolean, required = true, default = true, string = "Active")]
    pub active: bool,
    /// Source location override.
    #[field(
        many2one,
        comodel = "stock.location",
        ondelete = "restrict",
        index = "btree_not_null",
        string = "From"
    )]
    pub location_id: Option<RecId>,
    /// Destination location override.
    #[field(
        many2one,
        comodel = "stock.location",
        ondelete = "restrict",
        index = "btree_not_null",
        string = "To"
    )]
    pub location_dest_id: Option<RecId>,
    /// Source package override.
    #[field(
        many2one,
        comodel = "stock.package",
        ondelete = "restrict",
        index = "btree_not_null",
        string = "Source Package"
    )]
    pub package_id: Option<RecId>,
    /// Destination package override.
    #[field(
        many2one,
        comodel = "stock.package",
        ondelete = "restrict",
        index = "btree_not_null",
        string = "Destination Package"
    )]
    pub result_package_id: Option<RecId>,
    /// Owner override.
    #[field(
        many2one,
        comodel = "res.partner",
        ondelete = "restrict",
        index = "btree_not_null",
        string = "From Owner"
    )]
    pub owner_id: Option<RecId>,
}

/// `stock.move.line.correction`: one append-only correction of a done line (1).
///
/// Identified by `(move_line_id, seq)`; `seq` is dense per line across the line scope (no child) and
/// the child scopes (D4.1), which also makes's `(move_line_id, serial_child_id, seq)` unique.
/// Each tuple part is a column (`old_*`, `new_*`, D4.2): a child-mode line's line scope stores no
/// quantity or lot, a child scope its quantity 0 or 1, its lot and its overrides (null: follows the
/// line). Append-only is's trigger (amendment C1).
#[model(
    name = "stock.move.line.correction",
    description = "Move Line Correction",
    order = "move_line_id, seq",
    log_access = false,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    keys = [key(
        name = "_identity",
        fields = ["move_line_id", "seq"],
        message = "A correction with this sequence already exists for the move line."
    )],
    constraints = [
        (
            "_seq_positive",
            "CHECK (seq >= 1)",
            "The sequence of a correction starts at 1."
        ),
        (
            "_child_quantity_unit",
            "CHECK (serial_child_id IS NULL OR (old_quantity IN (0, 1) AND new_quantity IN (0, 1)) IS TRUE)",
            "A serial correction moves a quantity of 0 or 1."
        ),
        // 5
        (
            "_serial_kind_scope",
            "CHECK ((kind <> 'add_serial' OR (serial_child_id IS NOT NULL AND old_quantity = 0 AND new_quantity = 1)) AND (kind <> 'remove_serial' OR (serial_child_id IS NOT NULL AND old_quantity = 1 AND new_quantity = 0)))",
            "Adding a serial takes a serial row from 0 to 1, removing it from 1 to 0."
        ),
        (
            "_finite_old_quantity",
            "CHECK (old_quantity <> 'NaN' AND old_quantity <> 'Infinity' AND old_quantity <> '-Infinity')",
            "The old quantity of a correction must be a finite number."
        ),
        (
            "_finite_new_quantity",
            "CHECK (new_quantity <> 'NaN' AND new_quantity <> 'Infinity' AND new_quantity <> '-Infinity')",
            "The new quantity of a correction must be a finite number."
        ),
        (
            "_finite_quantity_delta_old_key",
            "CHECK (quantity_delta_old_key <> 'NaN' AND quantity_delta_old_key <> 'Infinity' AND quantity_delta_old_key <> '-Infinity')",
            "The quantity change at the old key of a correction must be a finite number."
        ),
        (
            "_finite_quantity_delta_new_key",
            "CHECK (quantity_delta_new_key <> 'NaN' AND quantity_delta_new_key <> 'Infinity' AND quantity_delta_new_key <> '-Infinity')",
            "The quantity change at the new key of a correction must be a finite number."
        ),
        (
            "_line_company_product",
            "FOREIGN KEY (move_line_id, company_id, product_id) REFERENCES stock_move_line (id, company_id, product_id)",
            "A correction has the company and the product of its move line."
        ),
        // also refuses moving a corrected serial row to another line (11)
        (
            "_child_line",
            "FOREIGN KEY (serial_child_id, move_line_id) REFERENCES stock_move_line_serial (id, line_id)",
            "A serial correction names a serial row of its own move line."
        ),
        (
            "_old_lot_product",
            "FOREIGN KEY (old_lot_id, product_id) REFERENCES stock_lot (id, product_id)",
            "The old lot or serial number of a correction belongs to another product."
        ),
        (
            "_new_lot_product",
            "FOREIGN KEY (new_lot_id, product_id) REFERENCES stock_lot (id, product_id)",
            "The new lot or serial number of a correction belongs to another product."
        ),
    ],
)]
pub struct StockMoveLineCorrection {
    /// Company of the line.
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        ondelete = "restrict",
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Product of the line.
    #[field(
        many2one,
        comodel = "product.product",
        required = true,
        ondelete = "restrict",
        check_company = true,
        string = "Product"
    )]
    pub product_id: Option<RecId>,
    /// The corrected line; a line with corrections cannot be deleted.
    #[field(
        many2one,
        comodel = "stock.move.line",
        required = true,
        ondelete = "restrict",
        index = true,
        string = "Move Line"
    )]
    pub move_line_id: Option<RecId>,
    /// The corrected serial row; empty for the line scope.
    #[field(
        many2one,
        comodel = "stock.move.line.serial",
        ondelete = "restrict",
        index = "btree_not_null",
        string = "Serial Row"
    )]
    pub serial_child_id: Option<RecId>,
    /// Position in the line's correction order, from 1.
    #[field(integer, required = true, string = "Sequence")]
    pub seq: i32,
    /// What the correction does (5).
    #[field(
        selection,
        selection = [
            ("edit", "Edit"),
            ("reset", "Reset"),
            ("redo", "Redo"),
            ("add_serial", "Add Serial"),
            ("remove_serial", "Remove Serial")
        ],
        required = true,
        string = "Kind"
    )]
    pub kind: Option<Arc<str>>,
    /// Quantity before.
    #[field(float, digits = 0, string = "Old Quantity")]
    pub old_quantity: Decimal,
    /// Quantity after.
    #[field(float, digits = 0, string = "New Quantity")]
    pub new_quantity: Decimal,
    /// Source location before.
    #[field(
        many2one,
        comodel = "stock.location",
        ondelete = "restrict",
        string = "Old From"
    )]
    pub old_location_id: Option<RecId>,
    /// Source location after.
    #[field(
        many2one,
        comodel = "stock.location",
        ondelete = "restrict",
        string = "New From"
    )]
    pub new_location_id: Option<RecId>,
    /// Destination location before.
    #[field(
        many2one,
        comodel = "stock.location",
        ondelete = "restrict",
        string = "Old To"
    )]
    pub old_location_dest_id: Option<RecId>,
    /// Destination location after.
    #[field(
        many2one,
        comodel = "stock.location",
        ondelete = "restrict",
        string = "New To"
    )]
    pub new_location_dest_id: Option<RecId>,
    /// Lot before.
    #[field(
        many2one,
        comodel = "stock.lot",
        ondelete = "restrict",
        string = "Old Lot/Serial Number"
    )]
    pub old_lot_id: Option<RecId>,
    /// Lot after.
    #[field(
        many2one,
        comodel = "stock.lot",
        ondelete = "restrict",
        string = "New Lot/Serial Number"
    )]
    pub new_lot_id: Option<RecId>,
    /// Source package before.
    #[field(
        many2one,
        comodel = "stock.package",
        ondelete = "restrict",
        string = "Old Source Package"
    )]
    pub old_package_id: Option<RecId>,
    /// Source package after.
    #[field(
        many2one,
        comodel = "stock.package",
        ondelete = "restrict",
        string = "New Source Package"
    )]
    pub new_package_id: Option<RecId>,
    /// Destination package before.
    #[field(
        many2one,
        comodel = "stock.package",
        ondelete = "restrict",
        string = "Old Destination Package"
    )]
    pub old_result_package_id: Option<RecId>,
    /// Destination package after.
    #[field(
        many2one,
        comodel = "stock.package",
        ondelete = "restrict",
        string = "New Destination Package"
    )]
    pub new_result_package_id: Option<RecId>,
    /// Owner before.
    #[field(
        many2one,
        comodel = "res.partner",
        ondelete = "restrict",
        string = "Old Owner"
    )]
    pub old_owner_id: Option<RecId>,
    /// Owner after.
    #[field(
        many2one,
        comodel = "res.partner",
        ondelete = "restrict",
        string = "New Owner"
    )]
    pub new_owner_id: Option<RecId>,
    /// Quantity change at the old quant key, in the product unit.
    #[field(float, digits = 0, string = "Quantity Change at Old Key")]
    pub quantity_delta_old_key: Decimal,
    /// Quantity change at the new quant key, in the product unit.
    #[field(float, digits = 0, string = "Quantity Change at New Key")]
    pub quantity_delta_new_key: Decimal,
    /// When the correction takes effect (8).
    #[field(datetime, required = true, string = "Effective At")]
    pub effective_at: Option<DateTime>,
    /// Transaction time of the append.
    #[field(
        datetime,
        required = true,
        default_fn = now,
        string = "Recorded At"
    )]
    pub recorded_at: Option<DateTime>,
    /// Why the correction was made.
    #[field(char, string = "Cause")]
    pub cause: Option<Arc<str>>,
}
