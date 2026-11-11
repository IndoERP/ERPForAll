//! `ir.sequence`: sequential document numbering (BASE §4.1 to §4.6).

use erp_sdk::prelude::*;

/// `ir.sequence`.
#[model(name = "ir.sequence", description = "Sequence", company_access = direct(field = "company_id", branch = "exact", empty = "allow"))]
pub struct IrSequence {
    /// Sequence name.
    #[field(char, required = true)]
    pub name: Option<Arc<str>>,
    /// Sequence code for lookup.
    #[field(char)]
    pub code: Option<Arc<str>>,
    /// Implementation: standard (PostgreSQL sequence) or no_gap (row lock).
    #[field(selection, selection = [("standard", "Standard"), ("no_gap", "No gap")], default = "standard", required = true)]
    pub implementation: Option<Arc<str>>,
    /// Wait mode for gapless (no_gap) row locking: 'nowait' (default) or 'lock_timeout'.
    #[field(
        selection,
        selection = [("nowait", "No Wait"), ("lock_timeout", "Lock Timeout")],
        default = "nowait"
    )]
    pub wait_mode: Option<Arc<str>>,
    /// Prefix template.
    #[field(char)]
    pub prefix: Option<Arc<str>>,
    /// Suffix template.
    #[field(char)]
    pub suffix: Option<Arc<str>>,
    /// Next number counter.
    #[field(integer, default = 1, required = true)]
    pub number_next: i32,
    /// Step increment.
    #[field(integer, default = 1, required = true)]
    pub number_increment: i32,
    /// Number of digits after left-padding with '0'.
    #[field(integer, default = 0, required = true)]
    pub padding: i32,
    /// Company scope.
    #[field(many2one, comodel = "res.company", ondelete = "cascade")]
    pub company_id: Option<RecId>,
    /// Whether to partition numbering by date ranges.
    #[field(boolean, default = false)]
    pub use_date_range: bool,
    /// Active flag.
    #[field(boolean, default = true)]
    pub active: bool,
    /// Subsequence date ranges.
    #[field(
        one2many,
        comodel = "ir.sequence.date_range",
        inverse_name = "sequence_id"
    )]
    pub date_range_ids: Vec<RecId>,
}

#[model(
    name = "ir.sequence.date_range",
    description = "Sequence Date Range",
    company_access = parent(edge = "sequence_id"),
    constraints = [(
        "_unique_range_per_sequence",
        "UNIQUE(sequence_id, date_from, date_to)",
        "You cannot create two date ranges for the same sequence with the same date range."
    )]
)]
pub struct IrSequenceDateRange {
    /// Date from.
    #[field(date, required = true)]
    pub date_from: Option<Date>,
    /// Date to.
    #[field(date, required = true)]
    pub date_to: Option<Date>,
    /// Main sequence.
    #[field(
        many2one,
        comodel = "ir.sequence",
        required = true,
        ondelete = "cascade"
    )]
    pub sequence_id: Option<RecId>,
    /// Next number counter for this date range.
    #[field(integer, default = 1, required = true)]
    pub number_next: i32,
}
