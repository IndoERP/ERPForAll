// SPDX-License-Identifier: MIT OR Apache-2.0
//! Schema only: /. No costing writer, trigger or grant.
//! Codec checks arrive with; no ORM access grants are provided for ordinary users.

use crate::recorded_now;
use erp_sdk::prelude::*;

/// Valuation FIFO Layer: and the approved valuation table note.
#[model(
    name = "stock.valuation.layer",
    table = "stk_cost_layer",
    description = "Valuation FIFO Layer",
    rec_name = "product_id",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    log_access = false,
    row_version = true,
    constraints = [
        ("_id_company_product", "UNIQUE (id, company_id, product_id)", "A FIFO layer is unique with its company and product."),
        ("_source_company_product", "FOREIGN KEY (source_id, company_id, product_id) REFERENCES stk_value_source (id, company_id, product_id) ON DELETE RESTRICT", "A valuation row must have its source company and product."),
        ("_lot_product", "FOREIGN KEY (lot_id, product_id) REFERENCES stock_lot (id, product_id) ON DELETE RESTRICT", "A valuation lot must belong to its product."),
        ("_finite_original_quantity", "CHECK (original_quantity <> 'NaN' AND original_quantity <> 'Infinity' AND original_quantity <> '-Infinity')", "original_quantity must be a finite number."),
        ("_finite_original_value", "CHECK (original_value <> 'NaN' AND original_value <> 'Infinity' AND original_value <> '-Infinity')", "original_value must be a finite number."),
        ("_finite_consumed_quantity", "CHECK (consumed_quantity <> 'NaN' AND consumed_quantity <> 'Infinity' AND consumed_quantity <> '-Infinity')", "consumed_quantity must be a finite number."),
        ("_finite_consumed_value", "CHECK (consumed_value <> 'NaN' AND consumed_value <> 'Infinity' AND consumed_value <> '-Infinity')", "consumed_value must be a finite number."),
        ("_finite_residual_num", "CHECK (residual_num <> 'NaN' AND residual_num <> 'Infinity' AND residual_num <> '-Infinity')", "residual_num must be a finite number."),
        ("_finite_residual_den", "CHECK (residual_den <> 'NaN' AND residual_den <> 'Infinity' AND residual_den <> '-Infinity')", "residual_den must be a finite number."),
        ("_original_nonzero", "CHECK (original_quantity <> 0)", "A FIFO layer must have nonzero original quantity."),
        ("_shortage_complete", "CHECK (((kind = 'shortage') = (shortage_source_id IS NOT NULL)) AND ((kind = 'shortage') = (shortage_policy IS NOT NULL)))", "Only shortage layers must have their shortage source and policy."),
        // Dedicated _finite_* constraints own non-finite refusal. This check sorts before
        // them alphabetically, so avoid masking their precise condition identifier.
        ("_consumed_within", "CHECK (CASE WHEN consumed_quantity IN ('NaN', 'Infinity', '-Infinity') OR original_quantity IN ('NaN', 'Infinity', '-Infinity') THEN TRUE ELSE abs(consumed_quantity) <= abs(original_quantity) AND (consumed_quantity = 0 OR sign(consumed_quantity) = sign(original_quantity)) END)", "FIFO consumed quantity must have its original sign and stay within the original quantity."),
    ],
    indexes = [
        ("_open_stack", "(company_id, product_id, lot_id, effective_time, id) WHERE consumed_quantity <> original_quantity"),
    ],
    unique_indexes = [
        ("_identity", "(source_id, lot_id, kind) NULLS NOT DISTINCT", "A FIFO source and lot have one layer of each kind."),
    ],
)]
#[non_exhaustive]
pub struct ValuationLayer {
    /// Company id.
    #[field(
        many2one,
        comodel = "res.company",
        ondelete = "restrict",
        required = true
    )]
    pub company_id: Option<RecId>,
    /// Product id.
    #[field(
        many2one,
        comodel = "product.product",
        ondelete = "restrict",
        required = true,
        check_company = true
    )]
    pub product_id: Option<RecId>,
    /// Lot id.
    #[field(many2one, comodel = "stock.lot", ondelete = "restrict")]
    pub lot_id: Option<RecId>,
    /// Source id.
    #[field(
        many2one,
        comodel = "stock.valuation.source",
        ondelete = "restrict",
        required = true
    )]
    pub source_id: Option<RecId>,
    /// Kind.
    #[field(selection, selection = [("receipt", "Receipt"), ("shortage", "Shortage")], required = true)]
    pub kind: Option<Arc<str>>,
    /// Effective time (naive UTC).
    #[field(datetime, required = true)]
    pub effective_time: Option<DateTime>,
    /// Exact original quantity.
    #[field(float, digits = 0, required = true)]
    pub original_quantity: Decimal,
    /// Exact original value.
    #[field(float, digits = 0, required = true)]
    pub original_value: Decimal,
    /// Exact consumed quantity.
    #[field(float, digits = 0, required = true, default = 0)]
    pub consumed_quantity: Decimal,
    /// Exact consumed value.
    #[field(float, digits = 0, required = true, default = 0)]
    pub consumed_value: Decimal,
    /// Exact residual num.
    #[field(float, digits = 0, required = true, default = 0)]
    pub residual_num: Decimal,
    /// Exact residual den.
    #[field(float, digits = 0, required = true, default = 1)]
    pub residual_den: Decimal,
    /// Shortage source id.
    #[field(many2one, comodel = "stock.valuation.source", ondelete = "restrict")]
    pub shortage_source_id: Option<RecId>,
    /// Reserved policy selection: no valid choices until declares them.
    #[field(selection, selection = [])]
    pub shortage_policy: Option<Arc<str>>,
    /// Generation.
    #[field(integer, required = true)]
    pub generation: i32,
}

/// Valuation FIFO Consumption: and the approved valuation table note.
#[model(
    name = "stock.valuation.layer.consumption",
    table = "stk_layer_consumption",
    description = "Valuation FIFO Consumption",
    rec_name = "product_id",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    log_access = false,
    constraints = [
        ("_layer_company_product", "FOREIGN KEY (layer_id, company_id, product_id) REFERENCES stk_cost_layer (id, company_id, product_id) ON DELETE RESTRICT", "A consumption link must have its layer company and product."),
        ("_source_company_product", "FOREIGN KEY (out_source_id, company_id, product_id) REFERENCES stk_value_source (id, company_id, product_id) ON DELETE RESTRICT", "A consumption link must have its outgoing source company and product."),
        ("_identity", "UNIQUE (layer_id, out_source_id, seq)", "A FIFO layer consumption sequence must be unique."),
        ("_finite_quantity", "CHECK (quantity <> 'NaN' AND quantity <> 'Infinity' AND quantity <> '-Infinity')", "quantity must be a finite number."),
        ("_finite_grid_value", "CHECK (grid_value <> 'NaN' AND grid_value <> 'Infinity' AND grid_value <> '-Infinity')", "grid_value must be a finite number."),
    ],
)]
#[non_exhaustive]
pub struct ValuationLayerConsumption {
    /// Layer id.
    #[field(
        many2one,
        comodel = "stock.valuation.layer",
        ondelete = "restrict",
        required = true
    )]
    pub layer_id: Option<RecId>,
    /// Out source id.
    #[field(
        many2one,
        comodel = "stock.valuation.source",
        ondelete = "restrict",
        required = true,
        index = true
    )]
    pub out_source_id: Option<RecId>,
    /// Seq.
    #[field(integer, required = true)]
    pub seq: i32,
    /// Company id.
    #[field(
        many2one,
        comodel = "res.company",
        ondelete = "restrict",
        required = true
    )]
    pub company_id: Option<RecId>,
    /// Product id.
    #[field(
        many2one,
        comodel = "product.product",
        ondelete = "restrict",
        required = true,
        check_company = true
    )]
    pub product_id: Option<RecId>,
    /// Exact quantity.
    #[field(float, digits = 0, required = true)]
    pub quantity: Decimal,
    /// Exact grid value.
    #[field(float, digits = 0, required = true)]
    pub grid_value: Decimal,
    /// Generation.
    #[field(integer, required = true)]
    pub generation: i32,
    /// Recorded at (naive UTC).
    #[field(datetime, required = true, default_fn = recorded_now)]
    pub recorded_at: Option<DateTime>,
}
