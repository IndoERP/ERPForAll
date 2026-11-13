// SPDX-License-Identifier: MIT OR Apache-2.0
//! Schema only: /. No costing writer, trigger or grant.
//! Codec checks arrive with; no ORM access grants are provided for ordinary users.

use crate::recorded_now;
use erp_sdk::prelude::*;

/// Valuation Cost State: and the approved valuation table note.
#[model(
    name = "stock.valuation.state",
    table = "stk_cost_state",
    description = "Valuation Cost State",
    rec_name = "product_id",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    log_access = false,
    row_version = true,
    constraints = [
        ("_lot_product", "FOREIGN KEY (lot_id, product_id) REFERENCES stock_lot (id, product_id) ON DELETE RESTRICT", "A valuation lot must belong to its product."),
        ("_finite_grid_value", "CHECK (grid_value <> 'NaN' AND grid_value <> 'Infinity' AND grid_value <> '-Infinity')", "grid_value must be a finite number."),
        ("_finite_quantity", "CHECK (quantity <> 'NaN' AND quantity <> 'Infinity' AND quantity <> '-Infinity')", "quantity must be a finite number."),
        ("_finite_currency_carry", "CHECK (currency_carry <> 'NaN' AND currency_carry <> 'Infinity' AND currency_carry <> '-Infinity')", "currency_carry must be a finite number."),
        ("_finite_currency_increment", "CHECK (currency_increment <> 'NaN' AND currency_increment <> 'Infinity' AND currency_increment <> '-Infinity')", "currency_increment must be a finite number."),
        ("_valued_through_pair", "CHECK ((valued_through_time IS NULL) = (valued_through_source_id IS NULL))", "A valuation cursor must have both its time and source."),
        ("_generation_nonneg", "CHECK (generation >= 0)", "A valuation state generation cannot be negative."),
    ],
    unique_indexes = [
        ("_key", "(company_id, product_id, lot_id) NULLS NOT DISTINCT", "A valuation CostKey has one state."),
    ],
)]
#[non_exhaustive]
pub struct ValuationState {
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
    /// Method.
    #[field(selection, selection = [("standard", "Standard"), ("average", "Average Cost"), ("fifo", "FIFO")], required = true)]
    pub method: Option<Arc<str>>,
    /// Policy.
    #[field(selection, selection = [("cost_scale_v1", "Cost Scale v1")], required = true)]
    pub policy: Option<Arc<str>>,
    /// Exact grid value.
    #[field(float, digits = 0, required = true)]
    pub grid_value: Decimal,
    /// Exact quantity.
    #[field(float, digits = 0, required = true)]
    pub quantity: Decimal,
    /// Exact currency carry.
    #[field(float, digits = 0, required = true)]
    pub currency_carry: Decimal,
    /// Currency id.
    #[field(
        many2one,
        comodel = "res.currency",
        ondelete = "restrict",
        required = true
    )]
    pub currency_id: Option<RecId>,
    /// Exact currency increment.
    #[field(float, digits = 0, required = true)]
    pub currency_increment: Decimal,
    /// Valued through time (naive UTC).
    #[field(datetime)]
    pub valued_through_time: Option<DateTime>,
    /// Valued through source id.
    #[field(many2one, comodel = "stock.valuation.source", ondelete = "restrict")]
    pub valued_through_source_id: Option<RecId>,
    /// Covered through source id.
    #[field(many2one, comodel = "stock.valuation.source", ondelete = "restrict")]
    pub covered_through_source_id: Option<RecId>,
    /// Generation.
    #[field(integer, required = true)]
    pub generation: i32,
}

/// Valuation Admission Control: and the approved valuation table note.
#[model(
    name = "stock.valuation.control",
    table = "stk_cost_control",
    description = "Valuation Admission Control",
    rec_name = "product_id",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    log_access = false,
    constraints = [
        ("_key", "UNIQUE (company_id, product_id)", "An admission key has one valuation control row."),
        ("_seal_complete", "CHECK ((sealed = (seal_token IS NOT NULL)) AND (sealed = (sealed_until IS NOT NULL)))", "A sealed admission key must have its token and deadline; an open key has neither."),
        ("_counters_nonneg", "CHECK (worker_epoch >= 0 AND seal_revision >= 0 AND publication_generation >= 0)", "Valuation control counters cannot be negative."),
    ],
    indexes = [
        ("_sealed", "(sealed_until) WHERE sealed"),
    ],
)]
#[non_exhaustive]
pub struct ValuationControl {
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
    /// Worker epoch.
    #[field(integer, required = true, default = 0)]
    pub worker_epoch: i32,
    /// Seal revision.
    #[field(integer, required = true, default = 0)]
    pub seal_revision: i32,
    /// Whether the admission key is sealed.
    #[field(boolean, required = true, default = false)]
    pub sealed: bool,
    /// Committed seal ownership token.
    #[field(uuid)]
    pub seal_token: Option<Uuid>,
    /// Seal deadline from the database clock.
    #[field(timestamptz)]
    pub sealed_until: Option<TimestampTz>,
    /// Publication generation.
    #[field(integer, required = true, default = 0)]
    pub publication_generation: i32,
    /// Drained source id.
    #[field(many2one, comodel = "stock.valuation.source", ondelete = "restrict")]
    pub drained_source_id: Option<RecId>,
    /// Recorded at (naive UTC).
    #[field(datetime, required = true, default_fn = recorded_now)]
    pub recorded_at: Option<DateTime>,
}

/// Valuation Cost Checkpoint: and the approved valuation table note.
#[model(
    name = "stock.valuation.checkpoint",
    table = "stk_cost_checkpoint",
    description = "Valuation Cost Checkpoint",
    rec_name = "product_id",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    log_access = false,
    constraints = [
        ("_source_company_product", "FOREIGN KEY (source_id, company_id, product_id) REFERENCES stk_value_source (id, company_id, product_id) ON DELETE RESTRICT", "A valuation row must have its source company and product."),
        ("_lot_product", "FOREIGN KEY (lot_id, product_id) REFERENCES stock_lot (id, product_id) ON DELETE RESTRICT", "A valuation lot must belong to its product."),
        ("_finite_grid_value", "CHECK (grid_value <> 'NaN' AND grid_value <> 'Infinity' AND grid_value <> '-Infinity')", "grid_value must be a finite number."),
        ("_finite_quantity", "CHECK (quantity <> 'NaN' AND quantity <> 'Infinity' AND quantity <> '-Infinity')", "quantity must be a finite number."),
        ("_finite_currency_carry", "CHECK (currency_carry <> 'NaN' AND currency_carry <> 'Infinity' AND currency_carry <> '-Infinity')", "currency_carry must be a finite number."),
        ("_finite_currency_increment", "CHECK (currency_increment <> 'NaN' AND currency_increment <> 'Infinity' AND currency_increment <> '-Infinity')", "currency_increment must be a finite number."),
        ("_generation_nonneg", "CHECK (generation >= 0)", "A valuation checkpoint generation cannot be negative."),
    ],
    unique_indexes = [
        ("_position", "(company_id, product_id, lot_id, effective_time, source_id) NULLS NOT DISTINCT", "A valuation CostKey has one checkpoint at an ordering position."),
    ],
)]
#[non_exhaustive]
pub struct ValuationCheckpoint {
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
    /// Effective time (naive UTC).
    #[field(datetime, required = true)]
    pub effective_time: Option<DateTime>,
    /// Source id.
    #[field(
        many2one,
        comodel = "stock.valuation.source",
        ondelete = "restrict",
        required = true
    )]
    pub source_id: Option<RecId>,
    /// Exact grid value.
    #[field(float, digits = 0, required = true)]
    pub grid_value: Decimal,
    /// Exact quantity.
    #[field(float, digits = 0, required = true)]
    pub quantity: Decimal,
    /// Exact currency carry.
    #[field(float, digits = 0, required = true)]
    pub currency_carry: Decimal,
    /// Currency id.
    #[field(
        many2one,
        comodel = "res.currency",
        ondelete = "restrict",
        required = true
    )]
    pub currency_id: Option<RecId>,
    /// Exact currency increment.
    #[field(float, digits = 0, required = true)]
    pub currency_increment: Decimal,
    /// Generation.
    #[field(integer, required = true)]
    pub generation: i32,
    /// Recorded at (naive UTC).
    #[field(datetime, required = true, default_fn = recorded_now)]
    pub recorded_at: Option<DateTime>,
}
