// SPDX-License-Identifier: MIT OR Apache-2.0
//! Schema only: /. No costing writer, trigger or grant.
//! Codec checks arrive with; no ORM access grants are provided for ordinary users.

use crate::recorded_now;
use erp_sdk::prelude::*;

/// Valuation Source: and the approved valuation table note.
#[model(
    name = "stock.valuation.source",
    table = "stk_value_source",
    description = "Valuation Source",
    rec_name = "product_id",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    id_sequence = "stk_value_source_seq",
    constraints = [
        ("_identity", "UNIQUE (company_id, origin_model, origin_id, component, source_revision, kind, cost_partition)", "A valuation source revision component must be unique."),
        ("_id_company_product", "UNIQUE (id, company_id, product_id)", "A valuation source is unique with its company and product."),
        ("_revision_nonneg", "CHECK (source_revision >= 0)", "A valuation source revision cannot be negative."),
        ("_affected_not_after", "CHECK (affected_from_time <= effective_time)", "The affected position cannot follow its source effective time."),
        ("_versions_positive", "CHECK (schema_version > 0 AND semantic_version > 0)", "Valuation source versions must be positive."),
    ],
    indexes = [
        ("_effective", "(company_id, product_id, effective_time, id)"),
    ],
)]
#[non_exhaustive]
pub struct ValuationSource {
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
    /// Origin model.
    #[field(char, required = true)]
    pub origin_model: Option<Arc<str>>,
    /// Polymorphic origin id; typed ledger links are on allocation rows.
    #[field(many2one_reference, model_field = "origin_model", required = true)]
    pub origin_id: i64,
    /// Component.
    #[field(char, required = true, default = "")]
    pub component: Option<Arc<str>>,
    /// Source revision.
    #[field(integer, required = true)]
    pub source_revision: i32,
    /// Kind.
    #[field(char, required = true)]
    pub kind: Option<Arc<str>>,
    /// Cost partition.
    #[field(char, required = true, default = "")]
    pub cost_partition: Option<Arc<str>>,
    /// Effective time (naive UTC).
    #[field(datetime, required = true)]
    pub effective_time: Option<DateTime>,
    /// Affected from time (naive UTC).
    #[field(datetime, required = true)]
    pub affected_from_time: Option<DateTime>,
    /// Affected from source id.
    #[field(many2one, comodel = "stock.valuation.source", ondelete = "restrict")]
    pub affected_from_source_id: Option<RecId>,
    /// Method.
    #[field(selection, selection = [("standard", "Standard"), ("average", "Average Cost"), ("fifo", "FIFO")], required = true)]
    pub method: Option<Arc<str>>,
    /// Policy.
    #[field(selection, selection = [("cost_scale_v1", "Cost Scale v1")], required = true)]
    pub policy: Option<Arc<str>>,
    /// Schema version.
    #[field(integer, required = true)]
    pub schema_version: i32,
    /// Semantic version.
    #[field(integer, required = true)]
    pub semantic_version: i32,
    /// Canonical request equality evidence; digest alone is insufficient.
    #[field(json, required = true)]
    pub request_canonical: Option<erp_sdk::serde_json::Value>,
    /// Request digest.
    #[field(char, required = true)]
    pub request_digest: Option<Arc<str>>,
}

/// Valuation Quantity Allocation: and the approved valuation table note.
#[model(
    name = "stock.valuation.source.alloc",
    table = "stk_value_source_alloc",
    description = "Valuation Quantity Allocation",
    rec_name = "product_id",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    log_access = false,
    constraints = [
        ("_identity", "UNIQUE (source_id, ordinal)", "A valuation allocation ordinal must be unique per source."),
        ("_source_company_product", "FOREIGN KEY (source_id, company_id, product_id) REFERENCES stk_value_source (id, company_id, product_id) ON DELETE RESTRICT", "A valuation row must have its source company and product."),
        ("_line_company_product", "FOREIGN KEY (move_line_id, company_id, product_id) REFERENCES stock_move_line (id, company_id, product_id) ON DELETE RESTRICT", "A valuation allocation must have its ledger line company and product."),
        ("_child_line", "FOREIGN KEY (serial_child_id, move_line_id) REFERENCES stock_move_line_serial (id, line_id) ON DELETE RESTRICT", "A valuation serial child must belong to its ledger line."),
        ("_lot_product", "FOREIGN KEY (lot_id, product_id) REFERENCES stock_lot (id, product_id) ON DELETE RESTRICT", "A valuation lot must belong to its product."),
        ("_finite_quantity", "CHECK (quantity <> 'NaN' AND quantity <> 'Infinity' AND quantity <> '-Infinity')", "quantity must be a finite number."),
        ("_ordinal_nonneg", "CHECK (ordinal >= 0)", "A valuation allocation ordinal cannot be negative."),
        ("_side_correction", "CHECK ((side = 'original') = (correction_id IS NULL))", "Only original allocations have no correction."),
    ],
    indexes = [
        ("_cost_key_source", "(company_id, product_id, lot_id, source_id)"),
    ],
)]
#[non_exhaustive]
pub struct ValuationSourceAlloc {
    /// Source id.
    #[field(
        many2one,
        comodel = "stock.valuation.source",
        ondelete = "restrict",
        required = true
    )]
    pub source_id: Option<RecId>,
    /// Ordinal.
    #[field(integer, required = true)]
    pub ordinal: i32,
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
    /// Move line id.
    #[field(
        many2one,
        comodel = "stock.move.line",
        ondelete = "restrict",
        required = true,
        index = true
    )]
    pub move_line_id: Option<RecId>,
    /// Serial child id.
    #[field(
        many2one,
        comodel = "stock.move.line.serial",
        ondelete = "restrict",
        index = "btree_not_null"
    )]
    pub serial_child_id: Option<RecId>,
    /// Correction id.
    #[field(
        many2one,
        comodel = "stock.move.line.correction",
        ondelete = "restrict",
        index = "btree_not_null"
    )]
    pub correction_id: Option<RecId>,
    /// Side.
    #[field(selection, selection = [("original", "Original"), ("old", "Old"), ("new", "New")], required = true)]
    pub side: Option<Arc<str>>,
    /// Exact quantity.
    #[field(float, digits = 0, required = true)]
    pub quantity: Decimal,
    /// Recorded at (naive UTC).
    #[field(datetime, required = true, default_fn = recorded_now)]
    pub recorded_at: Option<DateTime>,
}

/// Valuation Source Receipt: and the approved valuation table note.
#[model(
    name = "stock.valuation.receipt",
    table = "stk_value_receipt",
    description = "Valuation Source Receipt",
    rec_name = "product_id",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    log_access = false,
    constraints = [
        ("_source", "UNIQUE (source_id)", "A valuation source has one receipt."),
        ("_source_company_product", "FOREIGN KEY (source_id, company_id, product_id) REFERENCES stk_value_source (id, company_id, product_id) ON DELETE RESTRICT", "A valuation row must have its source company and product."),
        ("_applied_complete", "CHECK (((state = 'applied') = (applied_generation IS NOT NULL)) AND ((state = 'applied') = (applied_mode IS NOT NULL)) AND ((state = 'applied') = (applied_at IS NOT NULL)))", "An applied receipt must have its complete outcome; a pending receipt has none."),
        ("_generation_nonneg", "CHECK (applied_generation >= 0)", "An applied generation cannot be negative."),
    ],
    indexes = [
        ("_pending", "(company_id, product_id, source_id) WHERE state = 'pending'"),
    ],
)]
#[non_exhaustive]
pub struct ValuationReceipt {
    /// Source id.
    #[field(
        many2one,
        comodel = "stock.valuation.source",
        ondelete = "restrict",
        required = true
    )]
    pub source_id: Option<RecId>,
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
    /// State.
    #[field(selection, selection = [("pending", "Pending"), ("applied", "Applied")], required = true, default = "pending")]
    pub state: Option<Arc<str>>,
    /// Applied mode.
    #[field(selection, selection = [("inline", "Inline"), ("replay", "Replay")])]
    pub applied_mode: Option<Arc<str>>,
    /// Applied generation.
    #[field(integer)]
    pub applied_generation: i32,
    /// Applied at (naive UTC).
    #[field(datetime)]
    pub applied_at: Option<DateTime>,
    /// Recorded at (naive UTC).
    #[field(datetime, required = true, default_fn = recorded_now)]
    pub recorded_at: Option<DateTime>,
}
