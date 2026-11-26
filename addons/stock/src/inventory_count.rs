// SPDX-License-Identifier: MIT OR Apache-2.0
//! Physical inventory and cycle counts on `stock.quant`, using the company's configured
//! annual count date (`stock.annual_inventory_month` / `stock.annual_inventory_day`) and
//! per-location `cyclic_inventory_frequency`.

use erp_addon_base::ir_config_setting::read_values;
use erp_sdk::settings::SettingsScope;
use erp_sdk::{Operation, ValueMap, prelude::*};

use crate::move_lines::DeferSettle;
use crate::quant::StockQuant;
use crate::quant_engine::{QuantChange, QuantKey, apply_quant_changes_in_chunks};
use crate::scrap::default_stock_location;

const MONTH_KEY: &str = "stock.annual_inventory_month";
const DAY_KEY: &str = "stock.annual_inventory_day";

fn db_err(e: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::db("An inventory count record could not be read.").with_source(e)
}

/// Find or create the virtual inventory adjustment location (`usage = 'inventory'`, non-scrap).
pub(crate) async fn ensure_inventory_location(
    env: &Env,
    tx: &mut Transaction,
    company_id: i64,
) -> Result<i64> {
    let rows = tx
        .query(
            "SELECT id::int8 FROM stock_location \
             WHERE usage = 'inventory' AND active IS TRUE \
               AND (company_id = $1 OR company_id IS NULL) \
               AND lower(name) NOT LIKE '%scrap%' \
             ORDER BY company_id NULLS LAST, id LIMIT 1",
            &[&company_id],
        )
        .await?;
    if let Some(row) = rows.first() {
        return row.try_get(0).map_err(db_err);
    }
    let mut vals = RawVals::new();
    vals.insert("name".into(), Cell::Str("Inventory adjustment".into()));
    vals.insert("usage".into(), Cell::Str("inventory".into()));
    vals.insert("company_id".into(), Cell::Id(company_id));
    let created = env
        .sudo(true)
        .records("stock.location")?
        .create(tx, vec![vals])
        .await?;
    tx.flush().await?;
    created
        .ids()
        .into_iter()
        .next()
        .ok_or_else(|| Error::invariant("Created inventory adjustment location returned no id."))
}

/// Read the company's configured `(month, day)` for annual physical inventory counts.
pub(crate) async fn company_annual_count_date(
    tx: &mut Transaction,
    company_id: i64,
) -> Result<(u8, u8)> {
    let env = tx.env().sudo(true).with_company(company_id)?;
    let wire_company = u64::try_from(company_id)
        .ok()
        .and_then(|id| erp_sdk::views::wire::WireId::try_from(id).ok())
        .ok_or_else(|| Error::validation("Invalid company identifier."))?;
    let read = read_values(
        &env,
        tx,
        SettingsScope::Company {
            company_id: wire_company,
        },
        vec![MONTH_KEY.to_owned(), DAY_KEY.to_owned()],
    )
    .await?;
    let json = erp_sdk::serde_json::to_value(&read)
        .map_err(|e| Error::internal(format!("annual inventory settings: {e}")))?;
    let values = json["values"]
        .as_array()
        .ok_or_else(|| Error::internal("annual inventory settings response missing 'values'"))?;
    let month_entry = values
        .iter()
        .find(|v| v["key"] == MONTH_KEY)
        .ok_or_else(|| Error::internal(format!("setting '{MONTH_KEY}' missing from response")))?;
    let month = month_entry["value"]
        .as_str()
        .and_then(|s| s.parse::<u8>().ok())
        .filter(|m| (1..=12).contains(m))
        .ok_or_else(|| {
            Error::validation(format!(
                "setting '{MONTH_KEY}' must be a month between 1 and 12"
            ))
        })?;
    let day_entry = values
        .iter()
        .find(|v| v["key"] == DAY_KEY)
        .ok_or_else(|| Error::internal(format!("setting '{DAY_KEY}' missing from response")))?;
    let day = day_entry["value"]
        .as_i64()
        .and_then(|d| u8::try_from(d).ok())
        .filter(|d| (1..=31).contains(d))
        .ok_or_else(|| {
            Error::validation(format!(
                "setting '{DAY_KEY}' must be a day between 1 and 31"
            ))
        })?;
    Ok((month, day))
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
    match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Compute the next scheduled count date from the company's business date: if `location_id`
/// defines a positive `cyclic_inventory_frequency`, returns `today + frequency days`; otherwise
/// returns the next occurrence of the company's configured annual count month/day.
pub(crate) async fn next_scheduled_count_date(
    tx: &mut Transaction,
    company_id: i64,
    location_id: i64,
) -> Result<Date> {
    let today = erp_addon_base::company_calendar::business_date(tx, company_id).await?;
    let freq_rows = tx
        .query(
            "SELECT cyclic_inventory_frequency FROM stock_location WHERE id = $1",
            &[&location_id],
        )
        .await?;
    let freq: i32 = freq_rows
        .first()
        .map(|r| r.try_get(0))
        .transpose()
        .map_err(db_err)?
        .unwrap_or(0);
    if freq > 0 {
        return Ok(Date::from_days(today.days() + freq));
    }
    let (month, day) = company_annual_count_date(tx, company_id).await?;
    let (year, _, _) = today.ymd();
    let m = u32::from(month);
    let d_this = u32::from(day).min(days_in_month(year, m));
    let this_year = Date::from_ymd(year, m, d_this)
        .ok_or_else(|| Error::internal("Failed to construct annual count date."))?;
    if this_year > today {
        return Ok(this_year);
    }
    let next_year = year + 1;
    let d_next = u32::from(day).min(days_in_month(next_year, m));
    Date::from_ymd(next_year, m, d_next)
        .ok_or_else(|| Error::internal("Failed to construct next annual count date."))
}

/// Apply an exact inventory adjustment delta for a quant key via a done `is_inventory` stock move
/// and one quant-engine balance change.
#[allow(clippy::too_many_arguments)]
async fn post_inventory_adjustment(
    env: &Env,
    tx: &mut Transaction,
    company_id: i64,
    product_id: i64,
    location_id: i64,
    lot_id: Option<i64>,
    package_id: Option<i64>,
    owner_id: Option<i64>,
    diff_product_uom: Decimal,
    note: Option<&str>,
) -> Result<()> {
    if diff_product_uom.is_zero() {
        return Ok(());
    }
    let inv_loc = ensure_inventory_location(env, tx, company_id).await?;
    let uom_rows = tx
        .query(
            "SELECT pt.uom_id::int8, pt.is_storable FROM product_product pp \
             JOIN product_template pt ON pt.id = pp.product_tmpl_id WHERE pp.id = $1",
            &[&product_id],
        )
        .await?;
    let Some(uom_row) = uom_rows.first() else {
        return Err(Error::missing(
            "Product not found for inventory adjustment.",
        ));
    };
    let uom_id: i64 = uom_row.try_get(0).map_err(db_err)?;
    let is_storable: bool = uom_row.try_get(1).map_err(db_err)?;

    let (src_loc, dst_loc, abs_qty) = if diff_product_uom > Decimal::ZERO {
        (inv_loc, location_id, diff_product_uom)
    } else {
        (location_id, inv_loc, -diff_product_uom)
    };
    let label = note
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("Inventory Adjustment");

    let move_model = env.records("stock.move")?;
    let line_model = env.records("stock.move.line")?;
    let mut move_vals = RawVals::new();
    move_vals.insert("company_id".into(), Cell::Id(company_id));
    move_vals.insert("product_id".into(), Cell::Id(product_id));
    move_vals.insert("product_uom_qty".into(), Cell::Decimal(abs_qty));
    move_vals.insert("uom_id".into(), Cell::Id(uom_id));
    move_vals.insert("location_id".into(), Cell::Id(src_loc));
    move_vals.insert("location_dest_id".into(), Cell::Id(dst_loc));
    move_vals.insert("is_inventory".into(), Cell::Bool(true));
    move_vals.insert("inventory_name".into(), Cell::Str(label.into()));
    move_vals.insert("origin".into(), Cell::Str(label.into()));
    let created_move = move_model.create(tx, vec![move_vals]).await?;
    tx.flush().await?;
    let move_id = created_move
        .ids()
        .into_iter()
        .next()
        .ok_or_else(|| Error::invariant("Created inventory adjustment move returned no id."))?;

    let mut line_vals = RawVals::new();
    line_vals.insert("move_id".into(), Cell::Id(move_id));
    line_vals.insert("quantity".into(), Cell::Decimal(abs_qty));
    line_vals.insert("uom_id".into(), Cell::Id(uom_id));
    line_vals.insert("location_id".into(), Cell::Id(src_loc));
    line_vals.insert("location_dest_id".into(), Cell::Id(dst_loc));
    if let Some(lid) = lot_id {
        line_vals.insert("lot_id".into(), Cell::Id(lid));
    }
    if let Some(pkg) = package_id {
        if diff_product_uom > Decimal::ZERO {
            line_vals.insert("result_package_id".into(), Cell::Id(pkg));
        } else {
            line_vals.insert("package_id".into(), Cell::Id(pkg));
        }
    }
    if let Some(own) = owner_id {
        line_vals.insert("owner_id".into(), Cell::Id(own));
    }
    tx.extension::<DeferSettle>().active = true;
    let line_res = line_model.create(tx, vec![line_vals]).await;
    tx.extension::<DeferSettle>().active = false;
    let created_line = line_res?;
    tx.flush().await?;
    let line_id = created_line
        .ids()
        .into_iter()
        .next()
        .ok_or_else(|| Error::invariant("Created inventory adjustment line returned no id."))?;

    if is_storable {
        let loc_key = QuantKey::new(company_id, product_id, location_id)
            .lot_id(lot_id)
            .package_id(package_id)
            .owner_id(owner_id);
        let inv_key = QuantKey::new(company_id, product_id, inv_loc)
            .lot_id(lot_id)
            .owner_id(owner_id);
        apply_quant_changes_in_chunks(
            tx,
            &[
                QuantChange::new(loc_key, diff_product_uom, Decimal::ZERO),
                QuantChange::new(inv_key, -diff_product_uom, Decimal::ZERO),
            ],
        )
        .await?;
    }

    let uid = env.uid();
    tx.execute(
        "UPDATE stock_move_line SET membership = 'done', picked = true, \
                done_at = COALESCE(done_at, (now() AT TIME ZONE 'UTC')), \
                write_uid = $2, write_date = (now() AT TIME ZONE 'UTC') \
         WHERE id = $1",
        &[&line_id, &uid],
    )
    .await?;
    crate::move_lines::sync_moves(tx, &[move_id]).await?;
    tx.execute(
        "UPDATE stock_move SET state = 'done', picked = true, \
                date = (now() AT TIME ZONE 'UTC'), \
                done_at = COALESCE(done_at, (now() AT TIME ZONE 'UTC')), \
                write_uid = $2, write_date = (now() AT TIME ZONE 'UTC') \
         WHERE id = $1",
        &[&move_id, &uid],
    )
    .await?;
    Ok(())
}

#[methods(StockQuant, addon = "stock")]
impl StockQuant {
    #[compute(depends = ["quantity", "inventory_quantity", "inventory_quantity_set"])]
    async fn _compute_inventory_diff_quantity(
        this: &RecordSet<StockQuant>,
        tx: &mut Transaction,
        out: &mut Assign<'_, StockQuant>,
    ) -> Result<()> {
        let qtys = this.mapped(tx, StockQuant::QUANTITY).await?;
        let counted = this.mapped(tx, StockQuant::INVENTORY_QUANTITY).await?;
        let is_set = this.mapped(tx, StockQuant::INVENTORY_QUANTITY_SET).await?;
        for (index, ((qty, cnt), set)) in qtys.into_iter().zip(counted).zip(is_set).enumerate() {
            let diff = if set { cnt - qty } else { Decimal::ZERO };
            out.set(index, StockQuant::INVENTORY_DIFF_QUANTITY, diff)?;
        }
        Ok(())
    }

    /// Set the counted quantity to match current on-hand quantity and mark the row as counted.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_set_inventory_quantity(
        this: &RecordSet<StockQuant>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let records = this.as_records();
        records.check_access(tx, Operation::Write).await?;
        let ids = records.ids();
        if ids.is_empty() {
            return Ok(());
        }
        tx.flush().await?;
        let rows = tx
            .query(
                "SELECT id::int8, company_id::int8, location_id::int8, quantity, \
                        inventory_date IS NOT NULL \
                 FROM stock_quant WHERE id = ANY($1) ORDER BY id",
                &[&ids],
            )
            .await?;
        for row in rows {
            let id: i64 = row.try_get(0).map_err(db_err)?;
            let company_id: i64 = row.try_get(1).map_err(db_err)?;
            let location_id: i64 = row.try_get(2).map_err(db_err)?;
            let qty: Decimal = row.try_get(3).map_err(db_err)?;
            let has_inv_date: bool = row.try_get(4).map_err(db_err)?;
            let mut vals = RawVals::new();
            vals.insert("inventory_quantity".into(), Cell::Decimal(qty));
            vals.insert("inventory_quantity_set".into(), Cell::Bool(true));
            if !has_inv_date {
                let next_date = next_scheduled_count_date(tx, company_id, location_id).await?;
                vals.insert("inventory_date".into(), Cell::Date(next_date));
            }
            records.browse([id])?.write(tx, vals).await?;
        }
        Ok(())
    }

    /// Live `onchange` when entering `inventory_quantity`: marks the row as counted and updates
    /// `inventory_diff_quantity` immediately in the form/list UI. Clearing the field resets
    /// `inventory_quantity_set` so stock is never silently zeroed.
    #[onchange("inventory_quantity")]
    async fn _onchange_inventory_quantity(
        this: &RecordSet<StockQuant>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        let entry = this.as_records();
        let decode_opt_dec = |cell: Cell, field: &str| -> Result<Option<Decimal>> {
            match cell {
                Cell::Null | Cell::Bool(false) => Ok(None),
                Cell::Decimal(d) => Ok(Some(d)),
                Cell::Int(i) => Ok(Some(Decimal::from(i))),
                Cell::Float(f) => Decimal::try_from(f)
                    .map(Some)
                    .map_err(|_| Error::validation(format!("{field} must be a finite number."))),
                other => Err(Error::validation(format!(
                    "{field} must be a number, not {other:?}."
                ))),
            }
        };
        let qty = decode_opt_dec(entry.cell(tx, "quantity").await?, "quantity")?
            .unwrap_or(Decimal::ZERO);
        let counted = decode_opt_dec(
            entry.cell(tx, "inventory_quantity").await?,
            "inventory_quantity",
        )?;
        let mut vals = RawVals::new();
        match counted {
            Some(c) => {
                vals.insert("inventory_quantity_set".into(), Cell::Bool(true));
                vals.insert("inventory_diff_quantity".into(), Cell::Decimal(c - qty));
            }
            None => {
                vals.insert("inventory_quantity_set".into(), Cell::Bool(false));
                vals.insert("inventory_diff_quantity".into(), Cell::Decimal(Decimal::ZERO));
            }
        }
        entry.write_base(tx, vals).await?;
        Ok(OnchangeResult::none())
    }

    /// Clear the counted quantity on selected rows without changing on-hand stock.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_clear_inventory_quantity(
        this: &RecordSet<StockQuant>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let records = this.as_records();
        records.check_access(tx, Operation::Write).await?;
        if records.ids().is_empty() {
            return Ok(());
        }
        let mut vals = RawVals::new();
        vals.insert("inventory_quantity".into(), Cell::Decimal(Decimal::ZERO));
        vals.insert("inventory_quantity_set".into(), Cell::Bool(false));
        records.write(tx, vals).await
    }

    /// Schedule a physical count on selected stock rows using the location's cycle frequency or
    /// the company's configured annual count date.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_request_count(
        this: &RecordSet<StockQuant>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let records = this.as_records();
        records.check_access(tx, Operation::Write).await?;
        let ids = records.ids();
        if ids.is_empty() {
            return Ok(());
        }
        tx.flush().await?;
        let uid = records.env().uid();
        let rows = tx
            .query(
                "SELECT id::int8, company_id::int8, location_id::int8, user_id::int8 \
                 FROM stock_quant WHERE id = ANY($1) ORDER BY id",
                &[&ids],
            )
            .await?;
        for row in rows {
            let id: i64 = row.try_get(0).map_err(db_err)?;
            let company_id: i64 = row.try_get(1).map_err(db_err)?;
            let location_id: i64 = row.try_get(2).map_err(db_err)?;
            let assigned: Option<i64> = row.try_get(3).map_err(db_err)?;
            let next_date = next_scheduled_count_date(tx, company_id, location_id).await?;
            let mut vals = RawVals::new();
            vals.insert("inventory_date".into(), Cell::Date(next_date));
            if assigned.is_none() {
                vals.insert("user_id".into(), Cell::Id(uid));
            }
            records.browse([id])?.write(tx, vals).await?;
        }
        Ok(())
    }

    /// Apply counted inventory quantities on selected rows, posting exact inventory adjustment
    /// moves and advancing `inventory_date` to the next cycle/annual count date.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_apply_inventory(
        this: &RecordSet<StockQuant>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let records = this.as_records().clone();
        records.check_access(tx, Operation::Write).await?;
        if !records.env().su()
            && !tx
                .has_group(records.env(), "stock.group_stock_manager")
                .await?
        {
            return Err(Error::access(
                "Only an Inventory Administrator can apply physical inventory adjustments.",
            ));
        }
        let ids = records.ids();
        if ids.is_empty() {
            return Ok(());
        }
        tx.savepoint(async move |tx| {
            tx.flush().await?;
            // Order by canonical quant key (company, product, location, lot, package, owner)
            // so quant_engine lock ordering is strictly ascending across all applied rows.
            let rows = tx
                .query(
                    "SELECT id::int8, company_id::int8, product_id::int8, location_id::int8, \
                            lot_id::int8, package_id::int8, owner_id::int8, quantity, \
                            inventory_quantity, COALESCE(inventory_quantity_set, false), \
                            inventory_note \
                     FROM stock_quant WHERE id = ANY($1) \
                     ORDER BY company_id, product_id, location_id, \
                              lot_id NULLS LAST, package_id NULLS LAST, owner_id NULLS LAST",
                    &[&ids],
                )
                .await?;
            let env = records.env();
            for row in rows {
                let id: i64 = row.try_get(0).map_err(db_err)?;
                let company_id: i64 = row.try_get(1).map_err(db_err)?;
                let product_id: i64 = row.try_get(2).map_err(db_err)?;
                let location_id: i64 = row.try_get(3).map_err(db_err)?;
                let lot_id: Option<i64> = row.try_get(4).map_err(db_err)?;
                let package_id: Option<i64> = row.try_get(5).map_err(db_err)?;
                let owner_id: Option<i64> = row.try_get(6).map_err(db_err)?;
                let quantity: Decimal = row.try_get(7).map_err(db_err)?;
                let counted_opt: Option<Decimal> = row.try_get(8).map_err(db_err)?;
                let is_set: bool = row.try_get(9).map_err(db_err)?;
                let note: Option<String> = row.try_get(10).map_err(db_err)?;

                let counted = match (is_set, counted_opt) {
                    (true, None) => {
                        return Err(Error::validation(
                            "Counted quantity is missing on a stock row marked as counted.",
                        ));
                    }
                    (false, None) => continue,
                    (_, Some(c)) => c,
                };
                let effective_set = is_set || counted != Decimal::ZERO;
                if !effective_set {
                    continue;
                }
                let diff = counted - quantity;
                post_inventory_adjustment(
                    env,
                    tx,
                    company_id,
                    product_id,
                    location_id,
                    lot_id,
                    package_id,
                    owner_id,
                    diff,
                    note.as_deref(),
                )
                .await?;
                let next_date = next_scheduled_count_date(tx, company_id, location_id).await?;
                let mut reset_vals = RawVals::new();
                reset_vals.insert("inventory_quantity".into(), Cell::Decimal(Decimal::ZERO));
                reset_vals.insert("inventory_quantity_set".into(), Cell::Bool(false));
                reset_vals.insert("inventory_date".into(), Cell::Date(next_date));
                // The quant row may have been pruned if its final balance became zero.
                let still_exists: i64 = tx
                    .query(
                        "SELECT count(*)::int8 FROM stock_quant WHERE id = $1",
                        &[&id],
                    )
                    .await?
                    .first()
                    .ok_or_else(|| Error::internal("Quant existence check returned no row."))?
                    .try_get(0)
                    .map_err(db_err)?;
                if still_exists > 0 {
                    records.browse([id])?.write(tx, reset_vals).await?;
                }
            }
            Ok(())
        })
        .await
    }
}

/// Transient wizard to record a physical inventory count for any product/location/lot — including
/// creating initial stock balances where no quant row existed yet.
#[model(
    name = "stock.inventory.adjustment.new",
    description = "New Inventory Count",
    kind = "transient",
    transient_max_age_seconds = 3600,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    constraints = [
        (
            "_counted_nonneg",
            "CHECK (counted_quantity >= 0)",
            "Counted quantity cannot be negative."
        ),
    ],
)]
pub struct StockInventoryAdjustmentNew {
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        ondelete = "restrict",
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "product.product",
        required = true,
        ondelete = "restrict",
        check_company = true,
        string = "Product"
    )]
    pub product_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "stock.location",
        required = true,
        ondelete = "restrict",
        check_company = true,
        string = "Location"
    )]
    pub location_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "stock.lot",
        ondelete = "restrict",
        check_company = true,
        string = "Lot/Serial Number"
    )]
    pub lot_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "stock.package",
        ondelete = "restrict",
        string = "Package"
    )]
    pub package_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "res.partner",
        ondelete = "restrict",
        string = "Owner"
    )]
    pub owner_id: Option<RecId>,
    #[field(
        float,
        digits = "Product Unit",
        required = true,
        default = 0,
        string = "Counted Quantity"
    )]
    pub counted_quantity: Decimal,
    #[field(char, string = "Adjustment Note")]
    pub note: Option<Arc<str>>,
}

#[methods(StockInventoryAdjustmentNew, addon = "stock")]
impl StockInventoryAdjustmentNew {
    #[method(override)]
    async fn default_get(
        this: &RecordSet<StockInventoryAdjustmentNew>,
        tx: &mut Transaction,
        fields: Vec<String>,
        sup: Super,
    ) -> Result<ValueMap> {
        let mut out: ValueMap = sup.call(tx, this.as_records(), (fields.clone(),)).await?;
        let company_id = match out.get("company_id") {
            Some(Value::Int(id)) => Some(*id),
            _ => this.env().company_id()?,
        };
        if let Some(cid) = company_id {
            if fields.iter().any(|f| f == "company_id") && !out.contains_key("company_id") {
                out.insert("company_id".into(), Value::Int(cid));
            }
            if fields.iter().any(|f| f == "location_id")
                && !out.contains_key("location_id")
                && let Some(loc) = default_stock_location(tx, cid).await?
            {
                out.insert("location_id".into(), Value::Int(loc));
            }
        }
        Ok(out)
    }

    /// Apply the counted quantity for the selected product/location/lot and close the dialog.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_apply(
        this: &RecordSet<StockInventoryAdjustmentNew>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let records = this.as_records().clone();
        records.check_access(tx, Operation::Write).await?;
        if !records.env().su()
            && !tx
                .has_group(records.env(), "stock.group_stock_manager")
                .await?
        {
            return Err(Error::access(
                "Only an Inventory Administrator can apply physical inventory adjustments.",
            ));
        }
        let id = records.ensure_one()?.int()?;
        tx.savepoint(async move |tx| {
            tx.flush().await?;
            let rows = tx
                .query(
                    "SELECT company_id::int8, product_id::int8, location_id::int8, \
                            lot_id::int8, package_id::int8, owner_id::int8, \
                            counted_quantity, note \
                     FROM stock_inventory_adjustment_new WHERE id = $1",
                    &[&id],
                )
                .await?;
            let Some(row) = rows.first() else {
                return Err(Error::missing("Inventory count wizard record not found."));
            };
            let company_id: i64 = row.try_get(0).map_err(db_err)?;
            let product_id: i64 = row.try_get(1).map_err(db_err)?;
            let location_id: i64 = row.try_get(2).map_err(db_err)?;
            let lot_id: Option<i64> = row.try_get(3).map_err(db_err)?;
            let package_id: Option<i64> = row.try_get(4).map_err(db_err)?;
            let owner_id: Option<i64> = row.try_get(5).map_err(db_err)?;
            let counted: Decimal = row.try_get(6).map_err(db_err)?;
            let note: Option<String> = row.try_get(7).map_err(db_err)?;

            let current_rows = tx
                .query(
                    "SELECT quantity FROM stock_quant \
                     WHERE company_id = $1 AND product_id = $2 AND location_id = $3 \
                       AND lot_id IS NOT DISTINCT FROM $4 \
                       AND package_id IS NOT DISTINCT FROM $5 \
                       AND owner_id IS NOT DISTINCT FROM $6",
                    &[
                        &company_id,
                        &product_id,
                        &location_id,
                        &lot_id,
                        &package_id,
                        &owner_id,
                    ],
                )
                .await?;
            let current: Decimal = current_rows
                .first()
                .map(|r| r.try_get(0))
                .transpose()
                .map_err(db_err)?
                .unwrap_or(Decimal::ZERO);
            let diff = counted - current;
            let env = records.env();
            post_inventory_adjustment(
                env,
                tx,
                company_id,
                product_id,
                location_id,
                lot_id,
                package_id,
                owner_id,
                diff,
                note.as_deref(),
            )
            .await?;

            // Stamp next scheduled count date on the resulting quant row.
            let next_date = next_scheduled_count_date(tx, company_id, location_id).await?;
            let quant_rows = tx
                .query(
                    "SELECT id::int8 FROM stock_quant \
                     WHERE company_id = $1 AND product_id = $2 AND location_id = $3 \
                       AND lot_id IS NOT DISTINCT FROM $4 \
                       AND package_id IS NOT DISTINCT FROM $5 \
                       AND owner_id IS NOT DISTINCT FROM $6",
                    &[
                        &company_id,
                        &product_id,
                        &location_id,
                        &lot_id,
                        &package_id,
                        &owner_id,
                    ],
                )
                .await?;
            if let Some(qrow) = quant_rows.first() {
                let qid: i64 = qrow.try_get(0).map_err(db_err)?;
                let mut qvals = RawVals::new();
                qvals.insert("inventory_date".into(), Cell::Date(next_date));
                env.records("stock.quant")?
                    .browse([qid])?
                    .write(tx, qvals)
                    .await?;
            }
            Ok(Value::Map(
                ValueMap::from([
                    ("type".into(), Value::str("ir.actions.act_window")),
                    ("name".into(), Value::str("Physical Inventory")),
                    ("res_model".into(), Value::str("stock.quant")),
                    ("target".into(), Value::str("current")),
                    (
                        "views".into(),
                        Value::list([
                            Value::list([Value::False, Value::str("list")]),
                            Value::list([Value::False, Value::str("form")]),
                        ]),
                    ),
                    (
                        "domain".into(),
                        Value::list([Value::list([
                            Value::str("location_id.usage"),
                            Value::str("="),
                            Value::str("internal"),
                        ])]),
                    ),
                ])
                .into(),
            ))
        })
        .await
    }
}
