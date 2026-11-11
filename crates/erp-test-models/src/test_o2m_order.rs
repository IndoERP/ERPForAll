//! Fixture models for the one2many inverse read (PERF.WRITE-O2M): line models whose
//! order puts the inverse first, not at all, in the middle, or orders by another table's field
//! (another many2one, a two-level many2one, an `_inherits` parent's field: these keep a join); a
//! one2many with a field domain; a one2many over a non-stored related inverse of two hops; a
//! stored compute over one line model, a stored two-field compute group over another; and a
//! many2many owner read.

use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_o2m_order");

/// The owner of every line model.
#[model(
    name = "test_o2m_order.owner",
    description = "O2M Order Owner",
    order = "name, id",
    addon = "test_o2m_order"
)]
pub struct Owner {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(one2many, comodel = "test_o2m_order.lead", inverse_name = "owner_id")]
    pub lead_ids: X2many<Lead>,
    #[field(one2many, comodel = "test_o2m_order.seq", inverse_name = "owner_id")]
    pub seq_ids: X2many<Seq>,
    #[field(one2many, comodel = "test_o2m_order.mid", inverse_name = "owner_id")]
    pub mid_ids: X2many<Mid>,
    #[field(one2many, comodel = "test_o2m_order.tagged", inverse_name = "owner_id")]
    pub tagged_ids: X2many<Tagged>,
    #[field(one2many, comodel = "test_o2m_order.deep", inverse_name = "owner_id")]
    pub deep_ids: X2many<Deep>,
    #[field(one2many, comodel = "test_o2m_order.inh", inverse_name = "owner_id")]
    pub inh_ids: X2many<Inh>,
    /// The sequence lines above 1: a field domain with a literal (planned per call).
    #[field(
        one2many,
        comodel = "test_o2m_order.seq",
        inverse_name = "owner_id",
        domain = [("sequence", ">", 1)]
    )]
    pub high_seq_ids: X2many<Seq>,
    /// Over the non-stored related many2one `hop.owner_rel` (`via_id.owner_id`), no domain.
    #[field(one2many, comodel = "test_o2m_order.hop", inverse_name = "owner_rel")]
    pub hop_ids: X2many<Hop>,
    #[field(
        many2many,
        comodel = "test_o2m_order.tag",
        relation = "test_o2m_order_owner_tag_rel",
        column1 = "owner_id",
        column2 = "tag_id"
    )]
    pub tag_ids: X2many<Tag>,
    /// `sum(lead_ids.amount)`, stored: its recompute reads `lead_ids` (the compute-driven read).
    #[field(float, compute = "_compute_total", store)]
    pub total: f64,
}

#[methods(Owner, addon = "test_o2m_order")]
impl Owner {
    #[compute(depends = ["lead_ids.amount"])]
    fn _compute_total(batch: &mut Batch<'_, Owner>) -> Result<()> {
        for i in batch.indices() {
            let lines = batch.get(i, Owner::LEAD_IDS)?;
            let total: f64 = batch.values_of(Lead::AMOUNT, lines.ids())?.iter().sum();
            batch.set(i, Owner::TOTAL, total)?;
        }
        Ok(())
    }
}

/// A tag, ordered by name (a many2one to it in an order needs a join).
#[model(
    name = "test_o2m_order.tag",
    description = "O2M Order Tag",
    order = "name, id",
    addon = "test_o2m_order"
)]
pub struct Tag {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

#[model(
    name = "test_o2m_order.lead",
    description = "O2M Order Lead Line",
    order = "owner_id, sequence, id",
    addon = "test_o2m_order"
)]
pub struct Lead {
    #[field(
        many2one,
        comodel = "test_o2m_order.owner",
        required,
        ondelete = "cascade",
        index
    )]
    pub owner_id: Many2one<Owner>,
    #[field(integer)]
    pub sequence: i32,
    #[field(float)]
    pub amount: f64,
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// Order without the inverse.
#[model(
    name = "test_o2m_order.seq",
    description = "O2M Order Sequence Line",
    order = "sequence, id",
    addon = "test_o2m_order"
)]
pub struct Seq {
    #[field(
        many2one,
        comodel = "test_o2m_order.owner",
        required,
        ondelete = "cascade",
        index
    )]
    pub owner_id: Many2one<Owner>,
    #[field(integer)]
    pub sequence: i32,
    #[field(float)]
    pub amount: f64,
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// The inverse in the middle of the order, descending.
#[model(
    name = "test_o2m_order.mid",
    description = "O2M Order Middle Line",
    order = "sequence, owner_id desc, id",
    addon = "test_o2m_order"
)]
pub struct Mid {
    #[field(
        many2one,
        comodel = "test_o2m_order.owner",
        required,
        ondelete = "cascade",
        index
    )]
    pub owner_id: Many2one<Owner>,
    #[field(integer)]
    pub sequence: i32,
    #[field(float)]
    pub amount: f64,
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// Ordered by another many2one: the read keeps the join to the tag table.
#[model(
    name = "test_o2m_order.tagged",
    description = "O2M Order Tagged Line",
    order = "tag_id, id",
    addon = "test_o2m_order"
)]
pub struct Tagged {
    #[field(
        many2one,
        comodel = "test_o2m_order.owner",
        required,
        ondelete = "cascade",
        index
    )]
    pub owner_id: Many2one<Owner>,
    #[field(many2one, comodel = "test_o2m_order.tag")]
    pub tag_id: Option<RecId>,
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// A group of tags, ordered by its tag (a many2one ordered by its own many2one).
#[model(
    name = "test_o2m_order.grp",
    description = "O2M Order Tag Group",
    order = "tag_id, id",
    addon = "test_o2m_order"
)]
pub struct Grp {
    #[field(many2one, comodel = "test_o2m_order.tag")]
    pub tag_id: Option<RecId>,
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// Ordered by a many2one whose comodel is ordered by another many2one: two joins.
#[model(
    name = "test_o2m_order.deep",
    description = "O2M Order Deep Line",
    order = "grp_id, id",
    addon = "test_o2m_order"
)]
pub struct Deep {
    #[field(
        many2one,
        comodel = "test_o2m_order.owner",
        required,
        ondelete = "cascade",
        index
    )]
    pub owner_id: Many2one<Owner>,
    #[field(many2one, comodel = "test_o2m_order.grp")]
    pub grp_id: Option<RecId>,
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// The `_inherits` parent of [`Inh`].
#[model(
    name = "test_o2m_order.ranked",
    description = "O2M Order Ranked",
    addon = "test_o2m_order"
)]
pub struct Ranked {
    #[field(integer)]
    pub rank: i32,
}

/// Ordered by a field of its `_inherits` parent: the delegate join stays.
#[model(
    name = "test_o2m_order.inh",
    description = "O2M Order Inherits Line",
    order = "rank, id",
    inherits = [("test_o2m_order.ranked", "ranked_id")],
    addon = "test_o2m_order"
)]
pub struct Inh {
    #[field(
        many2one,
        comodel = "test_o2m_order.ranked",
        required,
        ondelete = "cascade"
    )]
    pub ranked_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "test_o2m_order.owner",
        required,
        ondelete = "cascade",
        index
    )]
    pub owner_id: Many2one<Owner>,
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// The middle hop of [`Hop`]'s related owner.
#[model(
    name = "test_o2m_order.via",
    description = "O2M Order Via",
    addon = "test_o2m_order"
)]
pub struct Via {
    #[field(many2one, comodel = "test_o2m_order.owner")]
    pub owner_id: Many2one<Owner>,
}

/// A line whose owner is a non-stored related many2one of two hops.
#[model(
    name = "test_o2m_order.hop",
    description = "O2M Order Hop Line",
    addon = "test_o2m_order"
)]
pub struct Hop {
    #[field(many2one, comodel = "test_o2m_order.via")]
    pub via_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "test_o2m_order.owner",
        related = "via_id.owner_id"
    )]
    pub owner_rel: Many2one<Owner>,
    #[field(char)]
    pub name: Option<Arc<str>>,
}

#[model(
    name = "test_o2m_order.pair",
    description = "O2M Order Pair",
    addon = "test_o2m_order"
)]
pub struct Pair {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(
        one2many,
        comodel = "test_o2m_order.pair_line",
        inverse_name = "pair_id"
    )]
    pub line_ids: X2many<PairLine>,
    #[field(float, compute = "_compute_stats", store)]
    pub sum_amount: f64,
    #[field(float, compute = "_compute_stats", store)]
    pub max_amount: f64,
}

#[methods(Pair, addon = "test_o2m_order")]
impl Pair {
    #[compute(depends = ["line_ids.amount"])]
    fn _compute_stats(batch: &mut Batch<'_, Pair>) -> Result<()> {
        for i in batch.indices() {
            let lines = batch.get(i, Pair::LINE_IDS)?;
            let amounts = batch.values_of(PairLine::AMOUNT, lines.ids())?;
            let sum: f64 = amounts.iter().sum();
            let max = amounts.iter().copied().fold(0.0_f64, f64::max);
            batch.set(i, Pair::SUM_AMOUNT, sum)?;
            batch.set(i, Pair::MAX_AMOUNT, max)?;
        }
        Ok(())
    }
}

/// A line of [`Pair`].
#[model(
    name = "test_o2m_order.pair_line",
    description = "O2M Order Pair Line",
    addon = "test_o2m_order"
)]
pub struct PairLine {
    #[field(
        many2one,
        comodel = "test_o2m_order.pair",
        required,
        ondelete = "cascade",
        index
    )]
    pub pair_id: Many2one<Pair>,
    #[field(float)]
    pub amount: f64,
}
