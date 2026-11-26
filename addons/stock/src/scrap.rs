// SPDX-License-Identifier: MIT OR Apache-2.0
//! Stock scrap (`stock.scrap`): record damaged or obsolete stock and move it out of inventory
//! into a virtual scrap location in one exact quant-engine call.

use std::collections::BTreeMap;

use erp_sdk::sequence::draw_by_code;
use erp_sdk::{CompanyId, Operation, ValueMap, prelude::*};

use crate::move_lines::DeferSettle;
use crate::picking::StockPicking;
use crate::quant_engine::{QuantChange, QuantKey, apply_quant_changes_in_chunks};

/// `stock.scrap`: a scrap order moving damaged/obsolete goods to a scrap location.
#[model(
    name = "stock.scrap",
    description = "Scrap",
    order = "id desc",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    constraints = [
        (
            "_scrap_qty_positive",
            "CHECK (scrap_qty > 0)",
            "Scrap quantity must be strictly positive."
        ),
        (
            "_finite_scrap_qty",
            "CHECK (scrap_qty <> 'NaN' AND scrap_qty <> 'Infinity' AND scrap_qty <> '-Infinity')",
            "Scrap quantity must be a finite number."
        ),
    ],
)]
pub struct StockScrap {
    /// Scrap reference assigned from the `stock.scrap` sequence.
    #[field(
        char,
        required = true,
        readonly = true,
        copy = false,
        default = "New",
        index = true,
        string = "Reference"
    )]
    pub name: Option<Arc<str>>,
    /// Source document (transfer, order, or manual note).
    #[field(char, string = "Source Document")]
    pub origin: Option<Arc<str>>,
    /// Product being scrapped.
    #[field(
        many2one,
        comodel = "product.product",
        required = true,
        ondelete = "restrict",
        check_company = true,
        string = "Product"
    )]
    pub product_id: Option<RecId>,
    /// Exact quantity to scrap in `product_uom_id`.
    #[field(
        float,
        digits = "Product Unit",
        required = true,
        default = 1,
        string = "Quantity"
    )]
    pub scrap_qty: Decimal,
    /// Unit of measure of `scrap_qty`.
    #[field(
        many2one,
        comodel = "uom.uom",
        required = true,
        ondelete = "restrict",
        string = "Unit"
    )]
    pub product_uom_id: Option<RecId>,
    /// Lot or serial number being scrapped (required for tracked products).
    #[field(
        many2one,
        comodel = "stock.lot",
        ondelete = "restrict",
        check_company = true,
        string = "Lot/Serial Number"
    )]
    pub lot_id: Option<RecId>,
    /// Source package holding the scrapped stock.
    #[field(
        many2one,
        comodel = "stock.package",
        ondelete = "restrict",
        string = "Package"
    )]
    pub package_id: Option<RecId>,
    /// Owner of consigned stock being scrapped.
    #[field(
        many2one,
        comodel = "res.partner",
        ondelete = "restrict",
        check_company = true,
        string = "Owner"
    )]
    pub owner_id: Option<RecId>,
    /// Optional transfer this scrap was created from.
    #[field(
        many2one,
        comodel = "stock.picking",
        ondelete = "set null",
        index = "btree_not_null",
        check_company = true,
        string = "Transfer"
    )]
    pub picking_id: Option<RecId>,
    /// Generated scrap stock move once validated.
    #[field(
        many2one,
        comodel = "stock.move",
        readonly = true,
        copy = false,
        ondelete = "restrict",
        string = "Scrap Move"
    )]
    pub move_id: Option<RecId>,
    /// Internal location the scrapped stock is taken from.
    #[field(
        many2one,
        comodel = "stock.location",
        required = true,
        ondelete = "restrict",
        check_company = true,
        string = "Source Location"
    )]
    pub location_id: Option<RecId>,
    /// Virtual scrap destination location (`usage = 'inventory'`).
    #[field(
        many2one,
        comodel = "stock.location",
        required = true,
        ondelete = "restrict",
        check_company = true,
        string = "Scrap Location"
    )]
    pub scrap_location_id: Option<RecId>,
    /// Lifecycle state of the scrap order.
    #[field(
        selection,
        selection = [("draft", "Draft"), ("done", "Done")],
        required = true,
        readonly = true,
        copy = false,
        default = "draft",
        string = "Status"
    )]
    pub state: Option<Arc<str>>,
    /// Timestamp when the scrap was validated.
    #[field(datetime, readonly = true, copy = false, string = "Date")]
    pub date_done: Option<DateTime>,
    /// Flag indicating whether downstream replenishment should replace the scrapped quantity.
    #[field(boolean, default = false, string = "Replenish Quantities")]
    pub should_replenish: bool,
    /// Owning company.
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        ondelete = "restrict",
        index = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
}

fn db_err(e: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::db("A stock scrap record could not be read.").with_source(e)
}

/// Find the default internal stock location for `company_id`.
pub(crate) async fn default_stock_location(
    tx: &mut Transaction,
    company_id: i64,
) -> Result<Option<i64>> {
    let rows = tx
        .query(
            "SELECT lot_stock_id::int8 FROM stock_warehouse \
             WHERE company_id = $1 AND active IS TRUE ORDER BY sequence, id LIMIT 1",
            &[&company_id],
        )
        .await?;
    if let Some(row) = rows.first() {
        return Ok(Some(row.try_get(0).map_err(db_err)?));
    }
    let fallback = tx
        .query(
            "SELECT id::int8 FROM stock_location \
             WHERE usage = 'internal' AND active IS TRUE \
               AND (company_id = $1 OR company_id IS NULL) \
             ORDER BY company_id NULLS LAST, id LIMIT 1",
            &[&company_id],
        )
        .await?;
    fallback
        .first()
        .map(|row| row.try_get(0))
        .transpose()
        .map_err(db_err)
}

/// Read an existing virtual scrap location (`usage = 'inventory'`, named `Scrap`) without writing.
async fn find_scrap_location(tx: &mut Transaction, company_id: i64) -> Result<Option<i64>> {
    let rows = tx
        .query(
            "SELECT id::int8 FROM stock_location \
             WHERE usage = 'inventory' AND active IS TRUE \
               AND (company_id = $1 OR company_id IS NULL) \
               AND lower(name) LIKE '%scrap%' \
             ORDER BY company_id NULLS LAST, id LIMIT 1",
            &[&company_id],
        )
        .await?;
    rows.first()
        .map(|row| row.try_get(0))
        .transpose()
        .map_err(db_err)
}

/// Find or create the virtual scrap location (`usage = 'inventory'`, named `Scrap`).
pub(crate) async fn ensure_scrap_location(
    env: &Env,
    tx: &mut Transaction,
    company_id: i64,
) -> Result<i64> {
    if let Some(existing) = find_scrap_location(tx, company_id).await? {
        return Ok(existing);
    }
    let mut vals = RawVals::new();
    vals.insert("name".into(), Cell::Str("Scrap".into()));
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
        .ok_or_else(|| Error::invariant("Created scrap location returned no id."))
}

async fn next_scrap_name(env: &Env, tx: &mut Transaction, company_id: i64) -> Result<String> {
    let company = CompanyId::new(company_id)?;
    let today = env.context_today()?;
    draw_by_code(tx, &env.sudo(true), "stock.scrap", company, Some(today)).await
}

#[methods(StockScrap, addon = "stock")]
impl StockScrap {
    #[method(override)]
    async fn default_get(
        this: &RecordSet<StockScrap>,
        tx: &mut Transaction,
        fields: Vec<String>,
        sup: Super,
    ) -> Result<ValueMap> {
        let mut out: ValueMap = sup.call(tx, this.as_records(), (fields.clone(),)).await?;
        let env = this.env();
        let company_id = match out.get("company_id") {
            Some(Value::Int(id)) => Some(*id),
            _ => env.company_id()?,
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
            if fields.iter().any(|f| f == "scrap_location_id")
                && !out.contains_key("scrap_location_id")
                && let Some(scrap_loc) = find_scrap_location(tx, cid).await?
            {
                out.insert("scrap_location_id".into(), Value::Int(scrap_loc));
            }
        }
        Ok(out)
    }

    #[method(override)]
    async fn create(
        this: &RecordSet<StockScrap>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        this.as_records()
            .check_access(tx, Operation::Create)
            .await?;
        let env = this.env();
        let active_company = env.company_id()?;
        for vals in &mut vals_list {
            let company_id = match vals.get("company_id") {
                Some(Cell::Id(id)) => *id,
                Some(Cell::Int(id)) => i64::from(*id),
                _ => {
                    let cid = active_company.ok_or_else(|| {
                        Error::validation("A company is required to create a scrap order.")
                    })?;
                    vals.insert("company_id".into(), Cell::Id(cid));
                    cid
                }
            };
            let needs_name = match vals.get("name") {
                None | Some(Cell::Null) => true,
                Some(Cell::Str(s)) => s.trim().is_empty() || s.as_ref() == "New",
                _ => false,
            };
            if needs_name {
                let name = next_scrap_name(env, tx, company_id).await?;
                vals.insert("name".into(), Cell::Str(name.into()));
            }
            if matches!(vals.get("product_uom_id"), None | Some(Cell::Null))
                && let Some(pid) = match vals.get("product_id") {
                    Some(Cell::Id(id)) => Some(*id),
                    Some(Cell::Int(id)) => Some(i64::from(*id)),
                    _ => None,
                }
            {
                let rows = tx
                    .query(
                        "SELECT pt.uom_id::int8 FROM product_product pp \
                         JOIN product_template pt ON pt.id = pp.product_tmpl_id \
                         WHERE pp.id = $1",
                        &[&pid],
                    )
                    .await?;
                if let Some(row) = rows.first() {
                    let uom_id: i64 = row.try_get(0).map_err(db_err)?;
                    vals.insert("product_uom_id".into(), Cell::Id(uom_id));
                }
            }
            if matches!(vals.get("location_id"), None | Some(Cell::Null))
                && let Some(loc) = default_stock_location(tx, company_id).await?
            {
                vals.insert("location_id".into(), Cell::Id(loc));
            }
            if matches!(vals.get("scrap_location_id"), None | Some(Cell::Null)) {
                let scrap_loc = ensure_scrap_location(env, tx, company_id).await?;
                vals.insert("scrap_location_id".into(), Cell::Id(scrap_loc));
            }
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<StockScrap>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        for forbidden in ["state", "move_id", "date_done"] {
            if vals.contains(forbidden) {
                return Err(Error::validation(format!(
                    "Field '{forbidden}' on a scrap order cannot be written directly."
                )));
            }
        }
        let ids = this.ids();
        if !ids.is_empty() {
            tx.flush().await?;
            let rows = tx
                .query(
                    "SELECT count(*)::int8 FROM stock_scrap WHERE id = ANY($1) AND state = 'done'",
                    &[&ids],
                )
                .await?;
            let done_count: i64 = rows
                .first()
                .ok_or_else(|| Error::internal("Scrap state check returned no row."))?
                .try_get(0)
                .map_err(db_err)?;
            if done_count > 0 {
                return Err(Error::validation(
                    "Validated scrap orders cannot be modified.",
                ));
            }
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }

    #[method(override)]
    async fn unlink(this: &RecordSet<StockScrap>, tx: &mut Transaction, sup: Super) -> Result<()> {
        this.as_records()
            .check_access(tx, Operation::Unlink)
            .await?;
        let ids = this.ids();
        if !ids.is_empty() {
            let rows = tx
                .query(
                    "SELECT count(*)::int8 FROM stock_scrap WHERE id = ANY($1) AND state = 'done'",
                    &[&ids],
                )
                .await?;
            let done_count: i64 = rows[0].try_get(0).map_err(db_err)?;
            if done_count > 0 {
                return Err(Error::user("You cannot delete a validated scrap order."));
            }
        }
        sup.call(tx, this.as_records(), ()).await
    }

    /// Validate draft scrap order(s), moving exact stock from `location_id` to `scrap_location_id`.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_validate(this: &RecordSet<StockScrap>, tx: &mut Transaction) -> Result<()> {
        let records = this.as_records().clone();
        records.check_access(tx, Operation::Write).await?;
        let ids = records.ids();
        if ids.is_empty() {
            return Ok(());
        }
        tx.savepoint(async move |tx| {
            tx.flush().await?;
            let rows = tx
                .query(
                    "SELECT s.id::int8, s.name, s.origin, s.company_id::int8, s.product_id::int8, \
                            s.scrap_qty, s.product_uom_id::int8, s.lot_id::int8, s.package_id::int8, \
                            s.owner_id::int8, s.picking_id::int8, s.location_id::int8, \
                            s.scrap_location_id::int8, s.state, s.should_replenish, \
                            pt.is_storable, pt.tracking \
                     FROM stock_scrap s \
                     JOIN product_product pp ON pp.id = s.product_id \
                     JOIN product_template pt ON pt.id = pp.product_tmpl_id \
                     WHERE s.id = ANY($1) ORDER BY s.id FOR UPDATE OF s",
                    &[&ids],
                )
                .await?;
            let env = records.env();
            let uid = env.uid();
            let move_model = env.records("stock.move")?;
            let line_model = env.records("stock.move.line")?;

            for row in rows {
                let scrap_id: i64 = row.try_get(0).map_err(db_err)?;
                let name: String = row.try_get(1).map_err(db_err)?;
                let origin: Option<String> = row.try_get(2).map_err(db_err)?;
                let company_id: i64 = row.try_get(3).map_err(db_err)?;
                let product_id: i64 = row.try_get(4).map_err(db_err)?;
                let scrap_qty: Decimal = row.try_get(5).map_err(db_err)?;
                let uom_id: i64 = row.try_get(6).map_err(db_err)?;
                let lot_id: Option<i64> = row.try_get(7).map_err(db_err)?;
                let package_id: Option<i64> = row.try_get(8).map_err(db_err)?;
                let owner_id: Option<i64> = row.try_get(9).map_err(db_err)?;
                let picking_id: Option<i64> = row.try_get(10).map_err(db_err)?;
                let location_id: i64 = row.try_get(11).map_err(db_err)?;
                let scrap_location_id: i64 = row.try_get(12).map_err(db_err)?;
                let state: String = row.try_get(13).map_err(db_err)?;
                let should_replenish: bool = row.try_get(14).map_err(db_err)?;
                let is_storable: bool = row.try_get(15).map_err(db_err)?;
                let tracking: Option<String> = row.try_get(16).map_err(db_err)?;

                if state == "done" {
                    continue;
                }
                if scrap_qty <= Decimal::ZERO {
                    return Err(Error::validation("Scrap quantity must be strictly positive."));
                }
                match tracking.as_deref() {
                    Some("lot") if lot_id.is_none() => {
                        return Err(Error::user(
                            "You need to specify a Lot/Serial Number before validating this scrap.",
                        ));
                    }
                    Some("serial") if lot_id.is_none() => {
                        return Err(Error::user(
                            "You need to specify a Serial Number before validating this scrap.",
                        ));
                    }
                    _ => {}
                }

                let mut move_vals = RawVals::new();
                move_vals.insert("company_id".into(), Cell::Id(company_id));
                move_vals.insert("product_id".into(), Cell::Id(product_id));
                move_vals.insert("product_uom_qty".into(), Cell::Decimal(scrap_qty));
                move_vals.insert("uom_id".into(), Cell::Id(uom_id));
                move_vals.insert("location_id".into(), Cell::Id(location_id));
                move_vals.insert("location_dest_id".into(), Cell::Id(scrap_location_id));
                move_vals.insert("is_scrap".into(), Cell::Bool(true));
                move_vals.insert(
                    "should_replenish_scrapped".into(),
                    Cell::Bool(should_replenish),
                );
                let move_origin = origin.unwrap_or_else(|| name.clone());
                move_vals.insert("origin".into(), Cell::Str(move_origin.into()));
                if let Some(pid) = picking_id {
                    // Only attach to open transfers; done/cancelled transfers keep scrap standalone
                    let p_state: Option<String> = tx
                        .query("SELECT state FROM stock_picking WHERE id = $1", &[&pid])
                        .await?
                        .first()
                        .map(|r| r.try_get(0))
                        .transpose()
                        .map_err(db_err)?;
                    if !matches!(p_state.as_deref(), Some("done" | "cancel")) {
                        move_vals.insert("picking_id".into(), Cell::Id(pid));
                    }
                }
                let created_move = move_model.create(tx, vec![move_vals]).await?;
                tx.flush().await?;
                let move_id = created_move
                    .ids()
                    .into_iter()
                    .next()
                    .ok_or_else(|| Error::invariant("Created scrap move returned no id."))?;

                let mut line_vals = RawVals::new();
                line_vals.insert("move_id".into(), Cell::Id(move_id));
                line_vals.insert("quantity".into(), Cell::Decimal(scrap_qty));
                line_vals.insert("uom_id".into(), Cell::Id(uom_id));
                line_vals.insert("location_id".into(), Cell::Id(location_id));
                line_vals.insert("location_dest_id".into(), Cell::Id(scrap_location_id));
                if let Some(lid) = lot_id {
                    line_vals.insert("lot_id".into(), Cell::Id(lid));
                }
                if let Some(pkg) = package_id {
                    line_vals.insert("package_id".into(), Cell::Id(pkg));
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
                    .ok_or_else(|| Error::invariant("Created scrap move line returned no id."))?;

                let qty_row = tx
                    .query(
                        "SELECT quantity_product_uom FROM stock_move_line WHERE id = $1",
                        &[&line_id],
                    )
                    .await?;
                let qty_product_uom: Decimal = qty_row
                    .first()
                    .ok_or_else(|| Error::internal("Scrap move line not found after create."))?
                    .try_get(0)
                    .map_err(db_err)?;
                if tracking.as_deref() == Some("serial") && qty_product_uom != Decimal::ONE {
                    return Err(Error::user(format!(
                        "A serial-tracked product must be scrapped in quantity 1 (base unit quantity was {qty_product_uom})."
                    )));
                }

                if is_storable && !qty_product_uom.is_zero() {
                    let src_key = QuantKey::new(company_id, product_id, location_id)
                        .lot_id(lot_id)
                        .package_id(package_id)
                        .owner_id(owner_id);
                    let dst_key = QuantKey::new(company_id, product_id, scrap_location_id)
                        .lot_id(lot_id)
                        .owner_id(owner_id);
                    apply_quant_changes_in_chunks(
                        tx,
                        &[
                            QuantChange::new(src_key, -qty_product_uom, Decimal::ZERO).as_floored(),
                            QuantChange::new(dst_key, qty_product_uom, Decimal::ZERO),
                        ],
                    )
                    .await?;
                }

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
                tx.execute(
                    "UPDATE stock_scrap SET state = 'done', move_id = $2, \
                            date_done = (now() AT TIME ZONE 'UTC'), \
                            write_uid = $3, write_date = (now() AT TIME ZONE 'UTC') \
                     WHERE id = $1",
                    &[&scrap_id, &move_id, &uid],
                )
                .await?;
            }
            records
                .modified_external(tx, &["state", "date_done", "write_date"])
                .await?;
            Ok(())
        })
        .await
    }
}

#[methods(StockPicking, addon = "stock")]
impl StockPicking {
    #[compute(depends = [], depends_context = ["uid", "allowed_company_ids"])]
    async fn _compute_scrap_count(
        this: &RecordSet<StockPicking>,
        tx: &mut Transaction,
        out: &mut Assign<'_, StockPicking>,
    ) -> Result<()> {
        let records = this.as_records();
        let ids = records.ids();
        let mut counts = BTreeMap::new();
        if !ids.is_empty() {
            let groups = this
                .env()
                .records("stock.scrap")?
                ._read_group(
                    tx,
                    ReadGroup::new(Domain::field("picking_id")?.in_(ids))
                        .groupby(["picking_id"])
                        .aggregates(["__count"]),
                )
                .await?;
            for (key, quantity) in groups.column(0).iter().zip(groups.column(1)) {
                if let (Some(id), GroupValue::Int(count)) = (key.record_id(), quantity) {
                    let count_i32 = i32::try_from(*count).map_err(|_| {
                        Error::invariant(format!("Scrap count {count} exceeds i32 range."))
                    })?;
                    counts.insert(id.get(), count_i32);
                }
            }
        }
        for (index, rec_id) in records.rec_ids().iter().enumerate() {
            let count = rec_id
                .as_real()
                .and_then(|id| counts.get(&id.get()).copied())
                .unwrap_or(0);
            out.set(index, StockPicking::SCRAP_COUNT, count)?;
        }
        Ok(())
    }

    /// Open a new Scrap form pre-filled from this transfer.
    #[method]
    #[rpc(effect = read)]
    pub async fn action_scrap(
        this: &RecordSet<StockPicking>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let records = this.as_records();
        records.check_access(tx, Operation::Read).await?;
        let id = records.ensure_one()?.int()?;
        let row = tx
            .query(
                "SELECT company_id::int8, location_id::int8, location_dest_id::int8, state, name \
                 FROM stock_picking WHERE id = $1",
                &[&id],
            )
            .await?;
        let Some(r) = row.first() else {
            return Err(Error::missing("Transfer not found."));
        };
        let company_id: i64 = r.try_get(0).map_err(db_err)?;
        let loc_src: i64 = r.try_get(1).map_err(db_err)?;
        let loc_dst: i64 = r.try_get(2).map_err(db_err)?;
        let state: String = r.try_get(3).map_err(db_err)?;
        let name: String = r.try_get(4).map_err(db_err)?;
        let source_loc = if state == "done" { loc_dst } else { loc_src };
        let ctx = ValueMap::from([
            ("default_picking_id".into(), Value::Int(id)),
            ("default_company_id".into(), Value::Int(company_id)),
            ("default_location_id".into(), Value::Int(source_loc)),
            ("default_origin".into(), Value::str(name)),
        ]);
        Ok(Value::Map(
            ValueMap::from([
                ("type".into(), Value::str("ir.actions.act_window")),
                ("name".into(), Value::str("Scrap")),
                ("res_model".into(), Value::str("stock.scrap")),
                (
                    "views".into(),
                    Value::list([Value::list([Value::False, Value::str("form")])]),
                ),
                ("target".into(), Value::str("new")),
                ("context".into(), Value::Map(ctx.into())),
            ])
            .into(),
        ))
    }

    /// Open scrap orders linked to this transfer.
    #[method]
    #[rpc(effect = read)]
    pub async fn action_view_scraps(
        this: &RecordSet<StockPicking>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let records = this.as_records();
        records.check_access(tx, Operation::Read).await?;
        let id = records.ensure_one()?.int()?;
        Ok(Value::Map(
            ValueMap::from([
                ("type".into(), Value::str("ir.actions.act_window")),
                ("name".into(), Value::str("Scraps")),
                ("res_model".into(), Value::str("stock.scrap")),
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
                        Value::str("picking_id"),
                        Value::str("="),
                        Value::Int(id),
                    ])]),
                ),
            ])
            .into(),
        ))
    }
}
