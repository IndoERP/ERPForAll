// SPDX-License-Identifier: MIT OR Apache-2.0
//! Addon `stock_landed_costs` for ERPForAll: landed cost documents, exact-decimal cost
//! allocation across incoming transfers (`equal`, `by_quantity`, `by_current_cost_price`,
//! `by_weight`, `by_volume`), valuation adjustment lines and product cost updates.

use std::collections::BTreeMap;

use erp_addon_product::ProductTemplate;
use erp_sdk::sequence::draw_by_code;
use erp_sdk::{CompanyId, Operation, ValueMap, prelude::*};

erp_sdk::addon!(
    name = "stock_landed_costs",
    exact_only = true,
    depends = ["base", "stock", "stock_account", "purchase_stock"]
);

fn db_err(e: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::db("A landed cost record could not be read.").with_source(e)
}

/// Split methods supported for landed cost allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitMethod {
    /// Split evenly across all target moves.
    Equal,
    /// Split proportionally to moved quantity.
    ByQuantity,
    /// Split proportionally to original move value.
    ByCurrentCost,
    /// Split proportionally to total move weight.
    ByWeight,
    /// Split proportionally to total move volume.
    ByVolume,
}

impl SplitMethod {
    /// Parse a selection value into a [`SplitMethod`].
    pub fn from_code(code: &str) -> Result<Self> {
        match code {
            "equal" => Ok(Self::Equal),
            "by_quantity" => Ok(Self::ByQuantity),
            "by_current_cost_price" => Ok(Self::ByCurrentCost),
            "by_weight" => Ok(Self::ByWeight),
            "by_volume" => Ok(Self::ByVolume),
            other => Err(Error::validation(format!(
                "Unknown landed cost split method '{other}'."
            ))),
        }
    }
}

/// Metrics of one target stock move eligible to receive landed cost allocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllocationTarget {
    /// Target `stock.move` id.
    pub move_id: i64,
    /// Target `product.product` id.
    pub product_id: i64,
    /// Done quantity moved.
    pub quantity: Decimal,
    /// Total weight (`quantity * product.weight`).
    pub weight: Decimal,
    /// Total volume (`quantity * product.volume`).
    pub volume: Decimal,
    /// Original cost (`quantity * unit_cost`).
    pub former_cost: Decimal,
}

/// Allocate `total_cost` across `targets` according to `method`, rounded to `scale` decimal
/// places while preserving exact total conservation (`sum(result) == total_cost.round_dp(scale)`).
pub fn allocate_landed_cost(
    total_cost: Decimal,
    method: SplitMethod,
    targets: &[AllocationTarget],
    scale: u32,
) -> Result<Vec<Decimal>> {
    if targets.is_empty() {
        return Err(Error::user(
            "Select at least one completed transfer with stock moves before computing landed costs.",
        ));
    }
    if total_cost < Decimal::ZERO {
        return Err(Error::validation("Landed cost amount cannot be negative."));
    }
    let rounded_total = total_cost.round_dp(scale);
    let weights: Vec<Decimal> = match method {
        SplitMethod::Equal => vec![Decimal::ONE; targets.len()],
        SplitMethod::ByQuantity => targets.iter().map(|t| t.quantity).collect(),
        SplitMethod::ByCurrentCost => targets.iter().map(|t| t.former_cost).collect(),
        SplitMethod::ByWeight => targets.iter().map(|t| t.weight).collect(),
        SplitMethod::ByVolume => targets.iter().map(|t| t.volume).collect(),
    };
    if weights.iter().any(|w| *w < Decimal::ZERO) {
        return Err(Error::validation(
            "Cannot split landed cost when a target move has a negative basis.",
        ));
    }
    let weight_sum: Decimal = weights.iter().copied().sum();
    if weight_sum <= Decimal::ZERO {
        return Err(Error::user(
            "Cannot split landed cost using the selected method because the total basis \
             (quantity, current cost, weight or volume) across the selected transfers is zero.",
        ));
    }
    let last_positive_idx = weights
        .iter()
        .rposition(|w| *w > Decimal::ZERO)
        .ok_or_else(|| {
            Error::user(
                "Cannot split landed cost using the selected method because no target move has a positive basis.",
            )
        })?;
    let mut allocated = Vec::with_capacity(targets.len());
    let mut running = Decimal::ZERO;
    for (idx, w) in weights.into_iter().enumerate() {
        if w.is_zero() {
            allocated.push(Decimal::ZERO);
        } else if idx == last_positive_idx {
            let remainder = (rounded_total - running).round_dp(scale);
            allocated.push(remainder);
        } else {
            let share = ((rounded_total * w) / weight_sum).round_dp(scale);
            running += share;
            allocated.push(share);
        }
    }
    Ok(allocated)
}

#[extend(ProductTemplate, addon = "stock_landed_costs")]
pub struct ProductTemplateLandedCost {
    /// Marks a service product as usable on landed cost lines.
    #[field(boolean, default = false, string = "Is a Landed Cost")]
    pub landed_cost_ok: bool,
    /// Default split method when this product is added to a landed cost document.
    #[field(
        selection,
        selection = [
            ("equal", "Equal"),
            ("by_quantity", "By Quantity"),
            ("by_current_cost_price", "By Current Cost"),
            ("by_weight", "By Weight"),
            ("by_volume", "By Volume")
        ],
        default = "equal",
        string = "Default Split Method"
    )]
    pub split_method_landed_cost: Option<Arc<str>>,
}

/// `stock.landed.cost`: document allocating additional acquisition costs (freight, customs,
/// insurance) onto completed stock transfers and updating product valuation.
#[model(
    name = "stock.landed.cost",
    description = "Stock Landed Cost",
    order = "date desc, id desc",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct StockLandedCost {
    /// Sequence reference (e.g. `LC/00001`).
    #[field(
        char,
        required = true,
        default = "New",
        copy = false,
        string = "Reference"
    )]
    pub name: Option<Arc<str>>,
    /// Accounting/valuation date.
    #[field(date, required = true, string = "Date")]
    pub date: Option<Date>,
    /// Owning company.
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        index = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Company currency for monetary amounts.
    #[field(
        many2one,
        comodel = "res.currency",
        related = "company_id.currency_id",
        store = true,
        string = "Currency"
    )]
    pub currency_id: Option<RecId>,
    /// Kind of target document (`picking`).
    #[field(
        selection,
        selection = [("picking", "Transfers")],
        default = "picking",
        required = true,
        string = "Apply On"
    )]
    pub target_model: Option<Arc<str>>,
    /// Transfers whose done moves receive the allocated landed costs.
    #[field(
        many2many,
        comodel = "stock.picking",
        relation = "stock_landed_cost_stock_picking_rel",
        column1 = "stock_landed_cost_id",
        column2 = "stock_picking_id",
        check_company = true,
        string = "Transfers"
    )]
    pub picking_ids: Vec<RecId>,
    /// Journal used for landed cost bookkeeping.
    #[field(
        many2one,
        comodel = "account.journal",
        check_company = true,
        string = "Account Journal"
    )]
    pub account_journal_id: Option<RecId>,
    /// Linked accounting journal entry when posted.
    #[field(
        many2one,
        comodel = "account.move",
        readonly = true,
        copy = false,
        string = "Journal Entry"
    )]
    pub account_move_id: Option<RecId>,
    /// Source vendor bill for these landed costs, if any.
    #[field(
        many2one,
        comodel = "account.move",
        check_company = true,
        string = "Vendor Bill"
    )]
    pub vendor_bill_id: Option<RecId>,
    /// Additional cost lines to allocate.
    #[field(
        one2many,
        comodel = "stock.landed.cost.lines",
        inverse_name = "cost_id",
        copy = true,
        string = "Additional Costs"
    )]
    pub cost_lines: Vec<RecId>,
    /// Computed valuation adjustment lines per transfer move.
    #[field(
        one2many,
        comodel = "stock.valuation.adjustment.lines",
        inverse_name = "cost_id",
        string = "Valuation Adjustments"
    )]
    pub valuation_adjustment_lines: Vec<RecId>,
    /// Total additional landed cost across all cost lines.
    #[field(
        monetary,
        currency_field = "currency_id",
        compute = "_compute_amount_total",
        store = true,
        string = "Total"
    )]
    pub amount_total: Decimal,
    /// Lifecycle status.
    #[field(
        selection,
        selection = [
            ("draft", "Draft"),
            ("done", "Posted"),
            ("cancel", "Cancelled")
        ],
        default = "draft",
        required = true,
        readonly = true,
        copy = false,
        string = "Status"
    )]
    pub state: Option<Arc<str>>,
    /// Free-text description.
    #[field(text, string = "Description")]
    pub description: Option<Arc<str>>,
}

/// `stock.landed.cost.lines`: one additional cost item on a landed cost document.
#[model(
    name = "stock.landed.cost.lines",
    description = "Stock Landed Cost Line",
    order = "id",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny", parent = "cost_id"),
    constraints = [(
        "_nonneg_price",
        "CHECK (price_unit >= 0)",
        "A landed cost line amount cannot be negative."
    )]
)]
pub struct StockLandedCostLine {
    /// Parent landed cost document.
    #[field(
        many2one,
        comodel = "stock.landed.cost",
        required = true,
        ondelete = "cascade",
        index = true,
        string = "Landed Cost"
    )]
    pub cost_id: Option<RecId>,
    /// Company inherited from the parent landed cost.
    #[field(
        many2one,
        comodel = "res.company",
        related = "cost_id.company_id",
        store = true,
        required = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Currency inherited from the parent landed cost.
    #[field(
        many2one,
        comodel = "res.currency",
        related = "cost_id.currency_id",
        store = true,
        string = "Currency"
    )]
    pub currency_id: Option<RecId>,
    /// Description shown on valuation adjustments.
    #[field(char, string = "Description")]
    pub name: Option<Arc<str>>,
    /// Service product representing this landed cost.
    #[field(
        many2one,
        comodel = "product.product",
        required = true,
        check_company = true,
        string = "Product"
    )]
    pub product_id: Option<RecId>,
    /// Expense/clearing account for this cost line.
    #[field(
        many2one,
        comodel = "account.account",
        check_company = true,
        string = "Account"
    )]
    pub account_id: Option<RecId>,
    /// Allocation rule for this line.
    #[field(
        selection,
        selection = [
            ("equal", "Equal"),
            ("by_quantity", "By Quantity"),
            ("by_current_cost_price", "By Current Cost"),
            ("by_weight", "By Weight"),
            ("by_volume", "By Volume")
        ],
        default = "equal",
        required = true,
        string = "Split Method"
    )]
    pub split_method: Option<Arc<str>>,
    /// Cost amount to allocate.
    #[field(
        monetary,
        currency_field = "currency_id",
        required = true,
        default = 0,
        string = "Cost"
    )]
    pub price_unit: Decimal,
}

/// `stock.valuation.adjustment.lines`: allocated landed cost per target stock move.
#[model(
    name = "stock.valuation.adjustment.lines",
    description = "Valuation Adjustment Line",
    order = "id",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny", parent = "cost_id")
)]
pub struct StockValuationAdjustmentLine {
    /// Parent landed cost document.
    #[field(
        many2one,
        comodel = "stock.landed.cost",
        required = true,
        ondelete = "cascade",
        index = true,
        string = "Landed Cost"
    )]
    pub cost_id: Option<RecId>,
    /// Source cost line.
    #[field(
        many2one,
        comodel = "stock.landed.cost.lines",
        ondelete = "cascade",
        string = "Cost Line"
    )]
    pub cost_line_id: Option<RecId>,
    /// Target stock move receiving the adjustment.
    #[field(
        many2one,
        comodel = "stock.move",
        ondelete = "set null",
        string = "Stock Move"
    )]
    pub move_id: Option<RecId>,
    /// Product on the target stock move.
    #[field(
        many2one,
        comodel = "product.product",
        required = true,
        check_company = true,
        string = "Product"
    )]
    pub product_id: Option<RecId>,
    /// Company inherited from parent landed cost.
    #[field(
        many2one,
        comodel = "res.company",
        related = "cost_id.company_id",
        store = true,
        required = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Currency inherited from parent landed cost.
    #[field(
        many2one,
        comodel = "res.currency",
        related = "cost_id.currency_id",
        store = true,
        string = "Currency"
    )]
    pub currency_id: Option<RecId>,
    /// Done quantity of the target move.
    #[field(float, digits = "Product Unit", default = 0, string = "Quantity")]
    pub quantity: Decimal,
    /// Total weight of the target move.
    #[field(float, digits = "Stock Weight", default = 0, string = "Weight")]
    pub weight: Decimal,
    /// Total volume of the target move.
    #[field(float, digits = 0, default = 0, string = "Volume")]
    pub volume: Decimal,
    /// Valuation before this landed cost adjustment.
    #[field(
        monetary,
        currency_field = "currency_id",
        default = 0,
        string = "Original Value"
    )]
    pub former_cost: Decimal,
    /// Allocated additional landed cost.
    #[field(
        monetary,
        currency_field = "currency_id",
        default = 0,
        string = "Additional Landed Cost"
    )]
    pub additional_landed_cost: Decimal,
    /// New valuation after adding the allocated landed cost.
    #[field(
        monetary,
        currency_field = "currency_id",
        default = 0,
        string = "New Value"
    )]
    pub final_cost: Decimal,
}

async fn next_landed_cost_name(
    env: &Env,
    tx: &mut Transaction,
    company_id: i64,
    date: Date,
) -> Result<String> {
    let company = CompanyId::new(company_id)?;
    draw_by_code(
        tx,
        &env.sudo(true),
        "stock.landed.cost",
        company,
        Some(date),
    )
    .await
}

async fn currency_decimal_places(tx: &mut Transaction, currency_id: Option<i64>) -> Result<u32> {
    let cid = currency_id
        .ok_or_else(|| Error::user("Landed cost company has no currency configured."))?;
    let rows = tx
        .query(
            "SELECT decimal_places::int4 FROM res_currency WHERE id = $1",
            &[&cid],
        )
        .await?;
    let row = rows
        .first()
        .ok_or_else(|| Error::missing("Landed cost currency was not found."))?;
    let dp: i32 = row.try_get(0).map_err(db_err)?;
    if !(0..=6).contains(&dp) {
        return Err(Error::validation(format!(
            "Unsupported currency decimal places '{dp}'."
        )));
    }
    u32::try_from(dp).map_err(|_| Error::internal("currency decimal_places conversion failed"))
}

async fn ensure_draft_costs(tx: &mut Transaction, cost_ids: &[i64], action: &str) -> Result<()> {
    if cost_ids.is_empty() {
        return Ok(());
    }
    let rows = tx
        .query(
            "SELECT id::int8, state FROM stock_landed_cost WHERE id = ANY($1) FOR UPDATE",
            &[&cost_ids],
        )
        .await?;
    for row in rows {
        let state: String = row.try_get(1).map_err(db_err)?;
        if state != "draft" {
            return Err(Error::user(format!(
                "Cannot {action} a posted or cancelled landed cost document."
            )));
        }
    }
    Ok(())
}

async fn recompute_adjustments(
    env: &Env,
    tx: &mut Transaction,
    cost_id: i64,
) -> Result<(Decimal, usize)> {
    tx.flush().await?;
    let header_rows = tx
        .query(
            "SELECT state, company_id::int8, currency_id::int8 \
             FROM stock_landed_cost WHERE id = $1 FOR UPDATE",
            &[&cost_id],
        )
        .await?;
    let Some(header) = header_rows.first() else {
        return Err(Error::missing("Landed cost document not found."));
    };
    let state: String = header.try_get(0).map_err(db_err)?;
    if state != "draft" {
        return Err(Error::user(
            "Only draft landed costs can be computed or recomputed.",
        ));
    }
    let company_id: i64 = header.try_get(1).map_err(db_err)?;
    let currency_id: Option<i64> = header.try_get(2).map_err(db_err)?;
    let scale = currency_decimal_places(tx, currency_id).await?;

    let cost_line_rows = tx
        .query(
            "SELECT id::int8, split_method, price_unit \
             FROM stock_landed_cost_lines \
             WHERE cost_id = $1 ORDER BY id",
            &[&cost_id],
        )
        .await?;
    if cost_line_rows.is_empty() {
        return Err(Error::user(
            "Add at least one additional cost line before computing landed costs.",
        ));
    }

    let picking_ids: Vec<i64> = tx
        .query(
            "SELECT stock_picking_id::int8 FROM stock_landed_cost_stock_picking_rel \
             WHERE stock_landed_cost_id = $1 ORDER BY stock_picking_id",
            &[&cost_id],
        )
        .await?
        .into_iter()
        .map(|r| r.try_get(0))
        .collect::<std::result::Result<Vec<i64>, _>>()
        .map_err(db_err)?;
    if picking_ids.is_empty() {
        return Err(Error::user(
            "Select at least one completed transfer before computing landed costs.",
        ));
    }

    let move_rows = tx
        .query(
            "SELECT m.id::int8, m.product_id::int8, m.quantity_product_uom, COALESCE(m.price_unit, 0), \
                    COALESCE(pt.weight, 0), COALESCE(pt.volume, 0) \
             FROM stock_move m \
             JOIN product_product pp ON pp.id = m.product_id \
             JOIN product_template pt ON pt.id = pp.product_tmpl_id \
             WHERE m.picking_id = ANY($1) AND m.company_id = $2 \
               AND m.state = 'done' AND m.quantity_product_uom > 0 \
             ORDER BY m.id",
            &[&picking_ids, &company_id],
        )
        .await?;
    if move_rows.is_empty() {
        return Err(Error::user(
            "The selected transfers have no completed stock moves with positive quantity.",
        ));
    }

    // Fall back to the product's standard_price when a move has price_unit == 0.
    let product_ids: Vec<i64> = move_rows
        .iter()
        .map(|r| r.try_get(1))
        .collect::<std::result::Result<Vec<i64>, _>>()
        .map_err(db_err)?;
    let mut std_prices: BTreeMap<i64, Decimal> = BTreeMap::new();
    let company_env = env.with_company(company_id)?;
    let prod_rows = company_env
        .records("product.product")?
        .browse(product_ids)?
        .read(tx, &["id", "standard_price"])
        .await?;
    for row in prod_rows {
        let Some(Value::Int(pid)) = row.get("id") else {
            return Err(Error::internal("product.product read missing integer id"));
        };
        let price = match row.get("standard_price") {
            Some(Value::Decimal(d)) => *d,
            Some(Value::Int(i)) => Decimal::from(*i),
            None | Some(Value::False) => Decimal::ZERO,
            other => {
                return Err(Error::internal(format!(
                    "Unexpected standard_price value for product {pid}: {other:?}"
                )));
            }
        };
        std_prices.insert(*pid, price);
    }

    let mut targets = Vec::with_capacity(move_rows.len());
    for row in move_rows {
        let move_id: i64 = row.try_get(0).map_err(db_err)?;
        let product_id: i64 = row.try_get(1).map_err(db_err)?;
        let quantity: Decimal = row.try_get(2).map_err(db_err)?;
        let move_price: Decimal = row.try_get(3).map_err(db_err)?;
        let unit_weight: Decimal = row.try_get(4).map_err(db_err)?;
        let unit_volume: Decimal = row.try_get(5).map_err(db_err)?;
        let unit_cost = if move_price > Decimal::ZERO {
            move_price
        } else {
            std_prices
                .get(&product_id)
                .copied()
                .unwrap_or(Decimal::ZERO)
        };
        let former_cost = (quantity * unit_cost).round_dp(scale);
        targets.push(AllocationTarget {
            move_id,
            product_id,
            quantity,
            weight: quantity * unit_weight,
            volume: quantity * unit_volume,
            former_cost,
        });
    }

    // Clear previous adjustment lines before creating the fresh allocation.
    let existing_adj_ids: Vec<i64> = tx
        .query(
            "SELECT id::int8 FROM stock_valuation_adjustment_lines WHERE cost_id = $1",
            &[&cost_id],
        )
        .await?
        .into_iter()
        .map(|r| r.try_get(0))
        .collect::<std::result::Result<Vec<i64>, _>>()
        .map_err(db_err)?;
    if !existing_adj_ids.is_empty() {
        env.records("stock.valuation.adjustment.lines")?
            .browse(existing_adj_ids)?
            .unlink(tx)
            .await?;
    }

    let mut to_create = Vec::new();
    let mut grand_total = Decimal::ZERO;
    for cl in cost_line_rows {
        let cost_line_id: i64 = cl.try_get(0).map_err(db_err)?;
        let method_code: String = cl.try_get(1).map_err(db_err)?;
        let line_cost: Decimal = cl.try_get(2).map_err(db_err)?;
        let method = SplitMethod::from_code(&method_code)?;
        let shares = allocate_landed_cost(line_cost, method, &targets, scale)?;
        for (target, additional) in targets.iter().zip(shares) {
            grand_total += additional;
            let final_cost = (target.former_cost + additional).round_dp(scale);
            let mut vals = RawVals::new();
            vals.insert("cost_id".into(), Cell::Id(cost_id));
            vals.insert("cost_line_id".into(), Cell::Id(cost_line_id));
            vals.insert("move_id".into(), Cell::Id(target.move_id));
            vals.insert("product_id".into(), Cell::Id(target.product_id));
            vals.insert("quantity".into(), Cell::Decimal(target.quantity));
            vals.insert("weight".into(), Cell::Decimal(target.weight));
            vals.insert("volume".into(), Cell::Decimal(target.volume));
            vals.insert("former_cost".into(), Cell::Decimal(target.former_cost));
            vals.insert("additional_landed_cost".into(), Cell::Decimal(additional));
            vals.insert("final_cost".into(), Cell::Decimal(final_cost));
            to_create.push(vals);
        }
    }

    let count = to_create.len();
    env.records("stock.valuation.adjustment.lines")?
        .create(tx, to_create)
        .await?;
    Ok((grand_total, count))
}

#[methods(StockLandedCostLine, addon = "stock_landed_costs")]
impl StockLandedCostLine {
    #[method(override)]
    async fn create(
        this: &RecordSet<StockLandedCostLine>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let env = this.env();
        let cost_ids: Vec<i64> = vals_list
            .iter()
            .filter_map(|v| match v.get("cost_id") {
                Some(Cell::Id(id)) => Some(*id),
                Some(Cell::Int(id)) => Some(i64::from(*id)),
                _ => None,
            })
            .collect();
        ensure_draft_costs(tx, &cost_ids, "add cost lines to").await?;
        for vals in &mut vals_list {
            let empty_name = match vals.get("name") {
                None | Some(Cell::Null) => true,
                Some(Cell::Str(s)) => s.trim().is_empty(),
                _ => false,
            };
            if empty_name {
                let prod_id = match vals.get("product_id") {
                    Some(Cell::Id(id)) => Some(*id),
                    Some(Cell::Int(id)) => Some(i64::from(*id)),
                    _ => None,
                };
                let label = if let Some(pid) = prod_id {
                    match env
                        .records("product.product")?
                        .browse(vec![pid])?
                        .cell(tx, "display_name")
                        .await?
                    {
                        Cell::Str(s) if !s.trim().is_empty() => s.to_string(),
                        _ => "Landed Cost".to_owned(),
                    }
                } else {
                    "Landed Cost".to_owned()
                };
                vals.insert("name".into(), Cell::Str(label.into()));
            }
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<StockLandedCostLine>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        let ids = this.ids();
        if !ids.is_empty() {
            let cost_ids: Vec<i64> = tx
                .query(
                    "SELECT DISTINCT cost_id::int8 FROM stock_landed_cost_lines WHERE id = ANY($1)",
                    &[&ids],
                )
                .await?
                .into_iter()
                .map(|r| r.try_get(0))
                .collect::<std::result::Result<Vec<i64>, _>>()
                .map_err(db_err)?;
            ensure_draft_costs(tx, &cost_ids, "modify cost lines on").await?;
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }

    #[method(override)]
    async fn unlink(
        this: &RecordSet<StockLandedCostLine>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        let ids = this.ids();
        if !ids.is_empty() {
            let cost_ids: Vec<i64> = tx
                .query(
                    "SELECT DISTINCT cost_id::int8 FROM stock_landed_cost_lines WHERE id = ANY($1)",
                    &[&ids],
                )
                .await?
                .into_iter()
                .map(|r| r.try_get(0))
                .collect::<std::result::Result<Vec<i64>, _>>()
                .map_err(db_err)?;
            ensure_draft_costs(tx, &cost_ids, "delete cost lines from").await?;
        }
        sup.call(tx, this.as_records(), ()).await
    }
}

#[methods(StockValuationAdjustmentLine, addon = "stock_landed_costs")]
impl StockValuationAdjustmentLine {
    #[method(override)]
    async fn write(
        this: &RecordSet<StockValuationAdjustmentLine>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        let ids = this.ids();
        if !ids.is_empty() {
            let cost_ids: Vec<i64> = tx
                .query(
                    "SELECT DISTINCT cost_id::int8 FROM stock_valuation_adjustment_lines WHERE id = ANY($1)",
                    &[&ids],
                )
                .await?
                .into_iter()
                .map(|r| r.try_get(0))
                .collect::<std::result::Result<Vec<i64>, _>>()
                .map_err(db_err)?;
            ensure_draft_costs(tx, &cost_ids, "modify valuation adjustments on").await?;
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }

    #[method(override)]
    async fn unlink(
        this: &RecordSet<StockValuationAdjustmentLine>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        let ids = this.ids();
        if !ids.is_empty() {
            let cost_ids: Vec<i64> = tx
                .query(
                    "SELECT DISTINCT cost_id::int8 FROM stock_valuation_adjustment_lines WHERE id = ANY($1)",
                    &[&ids],
                )
                .await?
                .into_iter()
                .map(|r| r.try_get(0))
                .collect::<std::result::Result<Vec<i64>, _>>()
                .map_err(db_err)?;
            ensure_draft_costs(tx, &cost_ids, "delete valuation adjustments from").await?;
        }
        sup.call(tx, this.as_records(), ()).await
    }
}

#[methods(StockLandedCost, addon = "stock_landed_costs")]
impl StockLandedCost {
    #[method(override)]
    async fn default_get(
        this: &RecordSet<StockLandedCost>,
        tx: &mut Transaction,
        fields: Vec<String>,
        sup: Super,
    ) -> Result<ValueMap> {
        let mut out: ValueMap = sup.call(tx, this.as_records(), (fields.clone(),)).await?;
        let active_company = this.env().company_id()?;
        if fields.iter().any(|f| f == "date") && !out.contains_key("date") {
            out.insert("date".into(), Value::Date(this.env().context_today()?));
        }
        if fields.iter().any(|f| f == "company_id")
            && !out.contains_key("company_id")
            && let Some(cid) = active_company
        {
            out.insert("company_id".into(), Value::Int(cid));
        }
        if fields.iter().any(|f| f == "account_journal_id")
            && !out.contains_key("account_journal_id")
            && let Some(cid) = active_company
        {
            let jrows = tx
                .query(
                    "SELECT id::int8 FROM account_journal \
                     WHERE company_id = $1 AND type = 'general' AND active IS TRUE \
                     ORDER BY sequence, id LIMIT 1",
                    &[&cid],
                )
                .await?;
            if let Some(jrow) = jrows.first() {
                let jid: i64 = jrow.try_get(0).map_err(db_err)?;
                out.insert("account_journal_id".into(), Value::Int(jid));
            }
        }
        Ok(out)
    }

    #[compute(depends = ["cost_lines", "cost_lines.price_unit"])]
    async fn _compute_amount_total(
        this: &RecordSet<StockLandedCost>,
        tx: &mut Transaction,
        out: &mut Assign<'_, StockLandedCost>,
    ) -> Result<()> {
        let rec_ids = this.as_records().rec_ids();
        let real_ids: Vec<i64> = rec_ids
            .iter()
            .filter_map(|id| id.as_real().map(|r| r.get()))
            .collect();
        let mut totals: BTreeMap<i64, Decimal> = BTreeMap::new();
        if !real_ids.is_empty() {
            tx.flush().await?;
            let rows = tx
                .query(
                    "SELECT cost_id::int8, COALESCE(sum(price_unit), 0) \
                     FROM stock_landed_cost_lines \
                     WHERE cost_id = ANY($1) GROUP BY cost_id",
                    &[&real_ids],
                )
                .await?;
            for row in rows {
                let cid: i64 = row.try_get(0).map_err(db_err)?;
                let sum: Decimal = row.try_get(1).map_err(db_err)?;
                totals.insert(cid, sum);
            }
        }
        for (index, rec_id) in rec_ids.iter().enumerate() {
            let total = rec_id
                .as_real()
                .and_then(|id| totals.get(&id.get()).copied())
                .unwrap_or(Decimal::ZERO);
            out.set(index, StockLandedCost::AMOUNT_TOTAL, total)?;
        }
        Ok(())
    }

    #[method(override)]
    async fn create(
        this: &RecordSet<StockLandedCost>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        this.as_records()
            .check_access(tx, Operation::Create)
            .await?;
        let env = this.env();
        let active_company = env.company_id()?;
        let today = env.context_today()?;
        for vals in &mut vals_list {
            if vals.contains("state") || vals.contains("account_move_id") {
                return Err(Error::validation(
                    "Landed cost status and journal entry are managed by posting actions.",
                ));
            }
            if matches!(vals.get("date"), None | Some(Cell::Null)) {
                vals.insert("date".into(), Cell::Date(today));
            }
            if matches!(vals.get("company_id"), None | Some(Cell::Null))
                && let Some(cid) = active_company
            {
                vals.insert("company_id".into(), Cell::Id(cid));
            }
            let company_id = match vals.get("company_id") {
                Some(Cell::Id(cid)) => *cid,
                Some(Cell::Int(cid)) => i64::from(*cid),
                _ => {
                    return Err(Error::validation(
                        "A landed cost document requires a company.",
                    ));
                }
            };
            let date = match vals.get("date") {
                Some(Cell::Date(d)) => *d,
                _ => today,
            };
            let needs_name = match vals.get("name") {
                None | Some(Cell::Null) => true,
                Some(Cell::Str(s)) => s.trim().is_empty() || s.as_ref() == "New",
                _ => false,
            };
            if needs_name {
                let name = next_landed_cost_name(env, tx, company_id, date).await?;
                vals.insert("name".into(), Cell::Str(name.into()));
            }
            if matches!(vals.get("account_journal_id"), None | Some(Cell::Null)) {
                let jrows = tx
                    .query(
                        "SELECT id::int8 FROM account_journal \
                         WHERE company_id = $1 AND type = 'general' AND active IS TRUE \
                         ORDER BY sequence, id LIMIT 1",
                        &[&company_id],
                    )
                    .await?;
                if let Some(jrow) = jrows.first() {
                    let jid: i64 = jrow.try_get(0).map_err(db_err)?;
                    vals.insert("account_journal_id".into(), Cell::Id(jid));
                }
            }
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<StockLandedCost>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        if vals.contains("state") || vals.contains("account_move_id") {
            return Err(Error::validation(
                "Landed cost status and journal entry are managed by posting actions.",
            ));
        }
        let ids = this.ids();
        ensure_draft_costs(tx, &ids, "modify").await?;
        sup.call(tx, this.as_records(), (vals,)).await
    }

    #[method(override)]
    async fn unlink(
        this: &RecordSet<StockLandedCost>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        let ids = this.ids();
        ensure_draft_costs(tx, &ids, "delete").await?;
        sup.call(tx, this.as_records(), ()).await
    }

    /// Compute or recompute valuation adjustment lines across the selected transfers.
    #[method]
    #[rpc(effect = write)]
    pub async fn compute_landed_cost(
        this: &RecordSet<StockLandedCost>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let records = this.as_records().clone();
        records.check_access(tx, Operation::Write).await?;
        let id = records.ensure_one()?.int()?;
        let env = records.env().clone();
        tx.savepoint(async move |tx| {
            let _ = recompute_adjustments(&env, tx, id).await?;
            Ok(())
        })
        .await
    }

    /// Post the landed cost document and apply unit-cost valuation adjustments to each product.
    #[method]
    #[rpc(effect = write)]
    pub async fn button_validate(
        this: &RecordSet<StockLandedCost>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let records = this.as_records().clone();
        records.check_access(tx, Operation::Write).await?;
        let id = records.ensure_one()?.int()?;
        let env = records.env().clone();
        tx.savepoint(async move |tx| {
            tx.flush().await?;
            let header = tx
                .query(
                    "SELECT state, company_id::int8, currency_id::int8 \
                     FROM stock_landed_cost WHERE id = $1 FOR UPDATE",
                    &[&id],
                )
                .await?;
            let Some(row) = header.first() else {
                return Err(Error::missing("Landed cost document not found."));
            };
            let state: String = row.try_get(0).map_err(db_err)?;
            let company_id: i64 = row.try_get(1).map_err(db_err)?;
            let currency_id: Option<i64> = row.try_get(2).map_err(db_err)?;
            if state != "draft" {
                return Err(Error::user("Only draft landed costs can be validated."));
            }
            let scale = currency_decimal_places(tx, currency_id).await?;

            // Always recompute adjustments against the current cost lines and transfers before posting.
            let _ = recompute_adjustments(&env, tx, id).await?;
            tx.flush().await?;

            // Deduplicate move quantities per (move_id, product_id) so multiple cost lines
            // do not multiply the denominator when computing unit cost uplift.
            let adj_rows = tx
                .query(
                    "WITH move_qty AS ( \
                         SELECT DISTINCT move_id, product_id, quantity \
                         FROM stock_valuation_adjustment_lines \
                         WHERE cost_id = $1 \
                     ), \
                     prod_qty AS ( \
                         SELECT product_id, COALESCE(sum(quantity), 0) AS moved_qty \
                         FROM move_qty \
                         GROUP BY product_id \
                     ), \
                     prod_cost AS ( \
                         SELECT product_id, COALESCE(sum(additional_landed_cost), 0) AS add_cost \
                         FROM stock_valuation_adjustment_lines \
                         WHERE cost_id = $1 \
                         GROUP BY product_id \
                     ) \
                     SELECT pc.product_id::int8, pq.moved_qty, pc.add_cost \
                     FROM prod_cost pc \
                     JOIN prod_qty pq ON pq.product_id = pc.product_id \
                     ORDER BY pc.product_id",
                    &[&id],
                )
                .await?;
            if adj_rows.is_empty() {
                return Err(Error::user(
                    "No valuation adjustment lines to post. Compute landed costs first.",
                ));
            }

            let company_env = env.with_company(company_id)?;
            for arow in adj_rows {
                let product_id: i64 = arow.try_get(0).map_err(db_err)?;
                let moved_qty: Decimal = arow.try_get(1).map_err(db_err)?;
                let add_cost: Decimal = arow.try_get(2).map_err(db_err)?;
                if add_cost == Decimal::ZERO {
                    continue;
                }
                let _ = tx
                    .query(
                        "SELECT id FROM product_product WHERE id = $1 FOR UPDATE",
                        &[&product_id],
                    )
                    .await?;
                let on_hand: Decimal = tx
                    .query(
                        "SELECT COALESCE(sum(q.quantity), 0) \
                         FROM stock_quant q \
                         JOIN stock_location l ON l.id = q.location_id \
                         WHERE q.company_id = $1 AND q.product_id = $2 AND l.usage = 'internal'",
                        &[&company_id, &product_id],
                    )
                    .await?
                    .first()
                    .ok_or_else(|| Error::internal("On-hand sum returned no row."))?
                    .try_get(0)
                    .map_err(db_err)?;
                let denom = if on_hand > moved_qty {
                    on_hand
                } else {
                    moved_qty
                };
                if denom <= Decimal::ZERO {
                    continue;
                }
                let prod_rec = company_env
                    .records("product.product")?
                    .browse(vec![product_id])?;
                let cur_price = match prod_rec.cell(tx, "standard_price").await? {
                    Cell::Decimal(d) => d,
                    Cell::Int(i) => Decimal::from(i),
                    Cell::Null => Decimal::ZERO,
                    other => {
                        return Err(Error::internal(format!(
                            "Unexpected standard_price cell for product {product_id}: {other:?}"
                        )));
                    }
                };
                let unit_uplift = (add_cost / denom).round_dp(scale);
                let new_price = (cur_price + unit_uplift).round_dp(scale);
                prod_rec
                    .write(
                        tx,
                        RawVals::new().set_cell("standard_price", Cell::Decimal(new_price)),
                    )
                    .await?;
            }

            let uid = env.uid();
            tx.execute(
                "UPDATE stock_landed_cost \
                 SET state = 'done', write_uid = $2, write_date = (now() AT TIME ZONE 'UTC') \
                 WHERE id = $1",
                &[&id, &uid],
            )
            .await?;
            records.modified_external(tx, &["state"]).await?;
            Ok(())
        })
        .await
    }

    /// Cancel a draft landed cost document.
    #[method]
    #[rpc(effect = write)]
    pub async fn button_cancel(
        this: &RecordSet<StockLandedCost>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let records = this.as_records().clone();
        records.check_access(tx, Operation::Write).await?;
        let id = records.ensure_one()?.int()?;
        let uid = records.env().uid();
        let rows = tx
            .query(
                "SELECT state FROM stock_landed_cost WHERE id = $1 FOR UPDATE",
                &[&id],
            )
            .await?;
        let state: String = rows
            .first()
            .ok_or_else(|| Error::missing("Landed cost document not found."))?
            .try_get(0)
            .map_err(db_err)?;
        if state != "draft" {
            return Err(Error::user(
                "Only draft landed cost documents can be cancelled.",
            ));
        }
        tx.execute(
            "UPDATE stock_landed_cost \
             SET state = 'cancel', write_uid = $2, write_date = (now() AT TIME ZONE 'UTC') \
             WHERE id = $1",
            &[&id, &uid],
        )
        .await?;
        records.modified_external(tx, &["state"]).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).expect("valid decimal literal")
    }

    fn sample_targets() -> Vec<AllocationTarget> {
        vec![
            AllocationTarget {
                move_id: 1,
                product_id: 10,
                quantity: dec("2"),
                weight: dec("1.5"),
                volume: dec("0.2"),
                former_cost: dec("200"),
            },
            AllocationTarget {
                move_id: 2,
                product_id: 11,
                quantity: dec("3"),
                weight: dec("4.5"),
                volume: dec("0.3"),
                former_cost: dec("300"),
            },
            AllocationTarget {
                move_id: 3,
                product_id: 12,
                quantity: dec("5"),
                weight: dec("4.0"),
                volume: dec("0.5"),
                former_cost: dec("500"),
            },
        ]
    }

    #[test]
    fn equal_split_conserves_exact_total_with_remainder() {
        let targets = sample_targets();
        let shares = allocate_landed_cost(dec("100.00"), SplitMethod::Equal, &targets, 2)
            .expect("equal split");
        assert_eq!(shares, vec![dec("33.33"), dec("33.33"), dec("33.34")]);
        let sum: Decimal = shares.into_iter().sum();
        assert_eq!(sum, dec("100.00"));
    }

    #[test]
    fn weighted_splits_preserve_exact_sum_for_qty_cost_weight_and_volume() {
        let targets = sample_targets();
        let qty = allocate_landed_cost(dec("1000.00"), SplitMethod::ByQuantity, &targets, 2)
            .expect("by_quantity");
        assert_eq!(qty, vec![dec("200.00"), dec("300.00"), dec("500.00")]);
        assert_eq!(qty.iter().copied().sum::<Decimal>(), dec("1000.00"));

        let cost = allocate_landed_cost(dec("500.00"), SplitMethod::ByCurrentCost, &targets, 2)
            .expect("by_current_cost_price");
        assert_eq!(cost, vec![dec("100.00"), dec("150.00"), dec("250.00")]);

        let weight = allocate_landed_cost(dec("100.00"), SplitMethod::ByWeight, &targets, 2)
            .expect("by_weight");
        assert_eq!(weight, vec![dec("15.00"), dec("45.00"), dec("40.00")]);

        let volume = allocate_landed_cost(dec("100.00"), SplitMethod::ByVolume, &targets, 2)
            .expect("by_volume");
        assert_eq!(volume, vec![dec("20.00"), dec("30.00"), dec("50.00")]);
    }

    #[test]
    fn zero_basis_refuses_split_loudly() {
        let mut targets = sample_targets();
        for t in &mut targets {
            t.weight = Decimal::ZERO;
        }
        let err = allocate_landed_cost(dec("100.00"), SplitMethod::ByWeight, &targets, 2)
            .expect_err("zero weight basis must fail");
        assert!(err.message().contains("total basis"));
    }
}
