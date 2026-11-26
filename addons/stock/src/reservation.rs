// SPDX-License-Identifier: MIT OR Apache-2.0
//! Reservation of moves: allocate free stock to waiting moves as open lines, and unreserve them
//! (D3, D6.3;.5).
//!
//! Observed in the reference (2138-2271`,
//! ): an unpicked move that is confirmed, waiting
//! or partially available reserves what it still misses, in the product unit; a move whose source
//! location bypasses reservation (vendor, customer, inventory, production) or whose product keeps
//! no stock gets one line for the whole missing quantity without touching balances; a make-to-order
//! move without origins waits; any other move takes free stock (on hand minus reserved, positive
//! only) from its source location and the locations below it, oldest incoming date first, into one
//! line per key, increasing an unpicked line of the same key instead of adding a second one. The
//! quantity taken is converted to the move's unit rounding down and back HALF-UP, so a line never
//! reserves more than was free. Unreserving deletes the unpicked lines of moves that are not picked,
//! done or cancelled; a done move cannot be unreserved.
//!
//! Adopted: those rules. Modified: the free stock is read once for the whole call
//! without a lock (the probe) and shared between the moves in priority, date and id order; the
//! chosen keys are then locked in key order by one engine call whose changes are marked probed, so
//! a key that a concurrent unit changed after the probe fails the unit as a retryable serialization
//! failure (`40001`, from the lock under `REPEATABLE READ`, or [`crate::QuantError::ChangedSinceProbe`]
//! from the floor) and the retry probes again. A short reservation is only ever the probe's answer,
//! never a lock's. At a location that forbids negative stock every growing reservation is floored
//! by the engine (3). The statements are fixed per call: the move locks, one read of
//! the moves, one probe, one read of the lines to increase, the ORM insert, one update, two reads
//! of the reservations, one engine call, the move quantity and state statements.
//!
//! Not here yet: moves with origin moves (chained reservation from what the origins brought,
//! W-I2.15) keep their state; removal strategies other than oldest first; the negative-sibling
//! offset of the reference; packaging rounding; the scheduler's chunked assignment and the
//! waiting queue per key.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use erp_addon_uom::stock::{ProductUnitSnapshot, StockRounding, product_quantity_sql};
use erp_sdk::Operation;
use erp_sdk::prelude::*;

use crate::error::StockError;
use crate::move_counters::{lock_move_roots, notify_pickings};
use crate::move_lines::{
    DeferSettle, LINE_DERIVED_MOVE_FIELDS, apply_reserved, recompute_states, reservation_changes,
    sync_moves,
};
use crate::move_units::{product_units, snapshots};
use crate::picking::StockPicking;
use crate::warehouse_setup::without_defaults;

/// The moves of `$1` that may reserve, in the order they reserve:
/// unpicked, confirmed, waiting or partially available. A receipt-linked return may reserve
/// from the completed source's destination. Other origin edges require chained reservation.
const READ_CANDIDATES: &str = "SELECT m.id, m.company_id, m.product_id, m.location_id, \
       m.location_dest_id, m.uom_id, m.product_uom_qty - m.quantity, \
       m.product_qty - m.quantity_product_uom, \
       m.procure_method = 'make_to_order' \
         AND NOT EXISTS (SELECT 1 FROM stock_move_move_rel r \
                         JOIN stock_move o ON o.id = r.move_orig_id \
                         WHERE r.move_dest_id = m.id AND o.state = 'done'), \
       EXISTS (SELECT 1 FROM stock_move_move_rel r \
               JOIN stock_move o ON o.id = r.move_orig_id \
               WHERE r.move_dest_id = m.id AND o.state NOT IN ('done', 'cancel') \
                 AND (source.id IS NULL OR r.move_orig_id <> source.id)), \
       l.usage IN ('supplier', 'customer', 'inventory', 'production') OR NOT pt.is_storable, \
       COALESCE(pt.tracking, 'none')::text \
     FROM stock_move m \
     LEFT JOIN stock_picking ret ON ret.id=m.picking_id AND ret.company_id=m.company_id \
     LEFT JOIN stock_move source ON source.id=m.origin_returned_move_id AND source.state='done' \
       AND source.picking_id=ret.return_id AND source.company_id=m.company_id \
       AND source.product_id=m.product_id AND source.uom_id=m.uom_id \
       AND source.location_dest_id=m.location_id AND source.is_scrap IS NOT TRUE \
     JOIN stock_location l ON l.id = m.location_id \
     JOIN product_product pp ON pp.id = m.product_id \
     JOIN product_template pt ON pt.id = pp.product_tmpl_id \
     WHERE m.id = ANY($1) AND NOT m.picked \
       AND m.state IN ('confirmed', 'waiting', 'partially_available') \
     ORDER BY m.priority DESC, m.date, m.id";

/// The probe (1): free stock of each `(company, product, location)` of the arrays, at the
/// location or below it, oldest incoming date first, matching the product's tracking policy
/// (lot-less for untracked products, lot/serial quants for tracked products). A plain read, no lock.
const PROBE: &str = "SELECT g.ord, q.location_id, q.lot_id, q.package_id, q.owner_id, \
       q.quantity - q.reserved_quantity \
     FROM unnest($1::int8[], $2::int8[], $3::int8[]) WITH ORDINALITY \
          AS g(company_id, product_id, location_id, ord) \
     JOIN stock_location root ON root.id = g.location_id \
     JOIN product_product pp ON pp.id = g.product_id \
     JOIN product_template pt ON pt.id = pp.product_tmpl_id \
     JOIN stock_quant q ON q.company_id = g.company_id AND q.product_id = g.product_id \
       AND ((COALESCE(pt.tracking, 'none') = 'none' AND q.lot_id IS NULL) \
            OR (COALESCE(pt.tracking, 'none') IN ('lot', 'serial') AND q.lot_id IS NOT NULL)) \
       AND q.quantity > 0 AND q.quantity > q.reserved_quantity \
     JOIN stock_location l ON l.id = q.location_id \
       AND l.parent_path LIKE root.parent_path || '%' \
     ORDER BY g.ord, q.in_date, q.id";

/// Unpicked open lines of the moves `$1` that a reservation of the same key increases:
/// the move's unit and destination, matching lot (never merging serial-tracked lines), no
/// pending `lot_name`, and no destination package.
const INCREASABLE: &str = "SELECT l.id, l.move_id, l.location_id, l.lot_id, l.package_id, l.owner_id \
     FROM stock_move_line l \
     JOIN stock_move m ON m.id = l.move_id \
     JOIN product_product pp ON pp.id = m.product_id \
     JOIN product_template pt ON pt.id = pp.product_tmpl_id \
     WHERE l.move_id = ANY($1) AND l.membership = 'open' AND l.done_at IS NULL AND l.active \
       AND NOT l.picked AND NOT l.child_mode AND l.lot_name IS NULL \
       AND COALESCE(pt.tracking, 'none') <> 'serial' \
       AND l.result_package_id IS NULL AND l.uom_id = m.uom_id \
       AND l.location_dest_id = m.location_dest_id";

/// The unpicked open lines of the moves `$1` that are not picked, done or cancelled.
const UNRESERVABLE: &str = "SELECT l.id FROM stock_move_line l JOIN stock_move m ON m.id = l.move_id \
     WHERE m.id = ANY($1) AND NOT m.picked AND m.state NOT IN ('done', 'cancel') \
       AND l.membership = 'open' AND l.active AND NOT l.picked";

/// Moves of `$1` that are done.
const DONE_MOVES: &str = "SELECT count(*) FROM stock_move WHERE id = ANY($1) AND state = 'done'";

/// The update that adds `$2` (text decimals, the move's unit) to the lines `$1`, with the
/// product-unit quantity by the unit service's SQL form over the line's own snapshot.
fn increase_sql() -> Result<String> {
    let product_quantity = product_quantity_sql("q", "uf", "puf", "d").map_err(|e| {
        Error::internal("The product-unit conversion of a move line could not be built.")
            .with_source(e)
    })?;
    Ok(format!(
        "WITH v AS ( \
           SELECT l.id, l.quantity + a.amount AS q, l.uom_factor AS uf, \
             l.product_uom_factor AS puf, l.product_unit_digits AS d \
           FROM unnest($1::int8[], $2::text[]::numeric[]) AS a(id, amount) \
           JOIN stock_move_line l ON l.id = a.id) \
         UPDATE stock_move_line l SET quantity = v.q, quantity_product_uom = {product_quantity} \
         FROM v WHERE l.id = v.id"
    ))
}

fn decode(e: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::db("A stock move, line or balance could not be read for a reservation.").with_source(e)
}

/// One move that may reserve.
struct Candidate {
    id: i64,
    company: i64,
    product: i64,
    location: i64,
    unit: i64,
    /// Still missing, in the move's unit.
    missing_unit: Decimal,
    /// Still missing, in the product unit.
    missing: Decimal,
    mto: bool,
    chained: bool,
    bypass: bool,
    tracking: String,
}

/// A free balance the probe found, shared by the moves that read it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct Slot {
    location: i64,
    lot: Option<i64>,
    package: Option<i64>,
    owner: Option<i64>,
}

/// A free slot of one company and product.
type SlotKey = (i64, i64, Slot);

/// A line key of a reservation: move, location, lot, package, owner.
type LineKey = (i64, i64, Option<i64>, Option<i64>, Option<i64>);

/// The probe's answer: the slots of each group in removal order, and the free quantity of each.
type Probed = (Vec<Vec<SlotKey>>, HashMap<SlotKey, Decimal>);

async fn count(tx: &mut Transaction, sql: &str, ids: &[i64]) -> Result<i64> {
    tx.query(sql, &[&ids])
        .await?
        .first()
        .ok_or_else(|| Error::internal("A count returned no row."))?
        .try_get(0)
        .map_err(decode)
}

async fn candidates(tx: &mut Transaction, moves: &[i64]) -> Result<Vec<Candidate>> {
    let mut out = Vec::new();
    for row in tx.query(READ_CANDIDATES, &[&moves]).await? {
        out.push(Candidate {
            id: row.try_get(0).map_err(decode)?,
            company: row.try_get(1).map_err(decode)?,
            product: row.try_get(2).map_err(decode)?,
            location: row.try_get(3).map_err(decode)?,
            unit: row.try_get(5).map_err(decode)?,
            missing_unit: row.try_get(6).map_err(decode)?,
            missing: row.try_get(7).map_err(decode)?,
            mto: row.try_get(8).map_err(decode)?,
            chained: row.try_get(9).map_err(decode)?,
            bypass: row.try_get(10).map_err(decode)?,
            tracking: row.try_get(11).map_err(decode)?,
        });
    }
    Ok(out)
}

/// The free slots of each `(company, product, location)` group, in removal order, and the free
/// quantity of every slot (shared across groups whose locations nest).
async fn probe(tx: &mut Transaction, groups: &[(i64, i64, i64)]) -> Result<Probed> {
    let mut lists = vec![Vec::new(); groups.len()];
    let mut free = HashMap::new();
    if groups.is_empty() {
        return Ok((lists, free));
    }
    let companies: Vec<i64> = groups.iter().map(|g| g.0).collect();
    let products: Vec<i64> = groups.iter().map(|g| g.1).collect();
    let locations: Vec<i64> = groups.iter().map(|g| g.2).collect();
    for row in tx
        .query(PROBE, &[&companies, &products, &locations])
        .await?
    {
        let ord: i64 = row.try_get(0).map_err(decode)?;
        let index = usize::try_from(ord - 1)
            .ok()
            .filter(|i| *i < groups.len())
            .ok_or_else(|| Error::internal("The reservation probe named no group."))?;
        let slot = Slot {
            location: row.try_get(1).map_err(decode)?,
            lot: row.try_get(2).map_err(decode)?,
            package: row.try_get(3).map_err(decode)?,
            owner: row.try_get(4).map_err(decode)?,
        };
        let (company, product, _) = groups[index];
        free.insert((company, product, slot), row.try_get(5).map_err(decode)?);
        lists[index].push((company, product, slot));
    }
    Ok((lists, free))
}

/// The largest quantity in the move's unit whose product-unit quantity does not exceed `take`:
/// HALF-UP back from the product unit, stepped down at the Product Unit digits when that rounded
/// up (rounds down, then back HALF-UP).
fn fit(snapshot: &ProductUnitSnapshot, take: Decimal) -> Result<(Decimal, Decimal)> {
    let mut line = snapshot.product_to_line(take, StockRounding::HalfUp)?;
    let step = Decimal::new(1, snapshot.digits());
    let mut product = snapshot.product_quantity(line)?;
    // shortcut: at most a few steps for any factor pair of one root; a loop bound keeps a defect
    // from spinning
    for _ in 0..4 {
        if product <= take || line <= Decimal::ZERO {
            break;
        }
        line -= step;
        product = snapshot.product_quantity(line)?;
    }
    if product > take {
        return Err(Error::internal(
            "A reservation could not fit the move unit into the free quantity.",
        ));
    }
    Ok((line.max(Decimal::ZERO), product.max(Decimal::ZERO)))
}

/// Moves that cannot
/// reserve (draft, done, cancelled, picked, covered) are left as they are; the others end
/// `assigned`, `partially_available`, `confirmed` or `waiting` by the state rule of the lines.
///
/// # Errors
///
/// A retryable `40001` when a chosen balance changed since the probe; the engine's refusals; read
/// and database errors.
pub(crate) async fn assign_moves(
    records: &Records,
    tx: &mut Transaction,
    moves: &[i64],
) -> Result<()> {
    if moves.is_empty() {
        return Ok(());
    }
    tx.flush().await?;
    let roots = lock_move_roots(tx, moves, &[]).await?;
    let candidates = candidates(tx, moves).await?;
    let env = records.env();
    let products: BTreeSet<i64> = candidates.iter().map(|c| c.product).collect();
    let product_unit = product_units(env, tx, &products).await?;
    let unit_of = |c: &Candidate| {
        product_unit
            .get(&c.product)
            .map(|p| (c.unit, *p))
            .ok_or_else(|| Error::internal("A product's unit was not read."))
    };
    let pairs = candidates
        .iter()
        .map(unit_of)
        .collect::<Result<BTreeSet<_>>>()?;
    let snapshots = snapshots(env, tx, pairs).await?;

    // what each move takes: (move, slot or none for a bypass line, quantity in the move unit)
    let mut takes: Vec<(i64, Option<Slot>, Decimal)> = Vec::new();
    let mut groups: Vec<(i64, i64, i64)> = Vec::new();
    let mut group_of: BTreeMap<(i64, i64, i64), usize> = BTreeMap::new();
    for c in &candidates {
        if c.missing <= Decimal::ZERO || c.chained {
            continue;
        }
        if c.bypass {
            takes.push((c.id, None, c.missing_unit));
            continue;
        }
        if c.mto {
            continue;
        }
        let key = (c.company, c.product, c.location);
        group_of.entry(key).or_insert_with(|| {
            groups.push(key);
            groups.len() - 1
        });
    }
    let (lists, mut free) = probe(tx, &groups).await?;
    for c in &candidates {
        if c.missing <= Decimal::ZERO || c.chained || c.bypass || c.mto {
            continue;
        }
        let Some(&index) = group_of.get(&(c.company, c.product, c.location)) else {
            continue;
        };
        let snapshot = snapshots
            .get(&unit_of(c)?)
            .ok_or_else(|| Error::internal("A unit snapshot was not captured."))?;
        let mut need = c.missing;
        let is_serial = c.tracking == "serial";
        for slot in &lists[index] {
            if need <= Decimal::ZERO {
                break;
            }
            let available = free.get(slot).copied().unwrap_or_default();
            if available <= Decimal::ZERO {
                continue;
            }
            let cap = if is_serial {
                if available < Decimal::ONE || need < Decimal::ONE {
                    continue;
                }
                Decimal::ONE
            } else {
                available.min(need)
            };
            let (line, taken) = fit(snapshot, cap)?;
            if line <= Decimal::ZERO || taken <= Decimal::ZERO {
                continue;
            }
            if is_serial && taken != Decimal::ONE {
                continue;
            }
            free.insert(*slot, available - taken);
            need -= taken;
            takes.push((c.id, Some(slot.2), line));
        }
    }
    let touched: Vec<i64> = candidates.iter().map(|c| c.id).collect();
    if takes.is_empty() {
        recompute_states(tx, &touched).await?;
        return notify(records, tx, &touched, &roots).await;
    }
    let reserving: BTreeSet<i64> = takes.iter().map(|t| t.0).collect();
    let reserving: Vec<i64> = reserving.into_iter().collect();

    // existing lines of the same key grow; the rest are new lines
    let location_of: HashMap<i64, i64> = candidates.iter().map(|c| (c.id, c.location)).collect();
    let mut increasable: HashMap<LineKey, i64> = HashMap::new();
    for row in tx.query(INCREASABLE, &[&reserving]).await? {
        let id: i64 = row.try_get(0).map_err(decode)?;
        let key = (
            row.try_get(1).map_err(decode)?,
            row.try_get(2).map_err(decode)?,
            row.try_get(3).map_err(decode)?,
            row.try_get(4).map_err(decode)?,
            row.try_get(5).map_err(decode)?,
        );
        increasable.entry(key).or_insert(id);
    }
    let mut grow: BTreeMap<i64, Decimal> = BTreeMap::new();
    let mut create: BTreeMap<LineKey, Decimal> = BTreeMap::new();
    for (move_id, slot, quantity) in takes {
        let (location, lot, package, owner) = match slot {
            Some(slot) => (slot.location, slot.lot, slot.package, slot.owner),
            None => (
                *location_of
                    .get(&move_id)
                    .ok_or_else(|| Error::internal("A reserving move was not read."))?,
                None,
                None,
                None,
            ),
        };
        let key = (move_id, location, lot, package, owner);
        match increasable.get(&key) {
            Some(line) => *grow.entry(*line).or_default() += quantity,
            None => *create.entry(key).or_default() += quantity,
        }
    }
    let grown: Vec<i64> = grow.keys().copied().collect();
    let mut nets = HashMap::new();
    reservation_changes(tx, &grown, Decimal::NEGATIVE_ONE, &mut nets).await?;
    if !grown.is_empty() {
        let adds: Vec<String> = grow.values().map(Decimal::to_string).collect();
        tx.execute(&increase_sql()?, &[&grown, &adds]).await?;
        let lines = env
            .records("stock.move.line")?
            .browse(grown.iter().copied())?;
        lines
            .modified_external(tx, &["quantity", "quantity_product_uom"])
            .await?;
    }
    let mut created = Vec::new();
    if !create.is_empty() {
        let vals_list: Vec<RawVals> = create
            .into_iter()
            .map(|((move_id, location, lot, package, owner), quantity)| {
                let mut vals = RawVals::new()
                    .set_cell("move_id", Cell::Id(move_id))
                    .set_cell("location_id", Cell::Id(location))
                    .set_cell("quantity", Cell::Decimal(quantity));
                if let Some(lot) = lot {
                    vals = vals.set_cell("lot_id", Cell::Id(lot));
                }
                if let Some(package) = package {
                    vals = vals.set_cell("package_id", Cell::Id(package));
                }
                if let Some(owner) = owner {
                    vals = vals.set_cell("owner_id", Cell::Id(owner));
                }
                vals
            })
            .collect();
        // the caller's `default_*` context belongs to the caller's own records, not to these lines
        let model = without_defaults(env).records("stock.move.line")?;
        tx.extension::<DeferSettle>().active = true;
        let result = model.create(tx, vals_list).await;
        tx.extension::<DeferSettle>().active = false;
        created = result?.ids();
        tx.flush().await?;
    }
    let mut changed = grown;
    changed.extend(created);
    reservation_changes(tx, &changed, Decimal::ONE, &mut nets).await?;
    apply_reserved(tx, nets, true).await?;
    sync_moves(tx, &reserving).await?;
    recompute_states(tx, &touched).await?;
    notify(records, tx, &touched, &roots).await
}

async fn notify(
    records: &Records,
    tx: &mut Transaction,
    moves: &[i64],
    roots: &[i64],
) -> Result<()> {
    records
        .env()
        .records("stock.move")?
        .browse(moves.iter().copied())?
        .modified_external(tx, &LINE_DERIVED_MOVE_FIELDS)
        .await?;
    notify_pickings(records, tx, roots).await
}

/// # Errors
///
/// [`StockError::UnreserveDone`] when a move is done; the line deletion's errors.
pub(crate) async fn unreserve_moves(
    records: &Records,
    tx: &mut Transaction,
    moves: &[i64],
) -> Result<()> {
    if moves.is_empty() {
        return Ok(());
    }
    tx.flush().await?;
    if count(tx, DONE_MOVES, moves).await? > 0 {
        return Err(StockError::UnreserveDone.into());
    }
    let mut lines = Vec::new();
    for row in tx.query(UNRESERVABLE, &[&moves]).await? {
        lines.push(row.try_get::<_, i64>(0).map_err(decode)?);
    }
    if lines.is_empty() {
        return Ok(());
    }
    records
        .env()
        .records("stock.move.line")?
        .browse(lines)?
        .unlink(tx)
        .await
}

/// The moves of the transfers `pickings`, ascending.
pub(crate) async fn picking_moves(tx: &mut Transaction, pickings: &[i64]) -> Result<Vec<i64>> {
    let mut out = Vec::new();
    for row in tx
        .query(
            "SELECT id FROM stock_move WHERE picking_id = ANY($1) AND is_scrap IS NOT TRUE ORDER BY id",
            &[&pickings],
        )
        .await?
    {
        out.push(row.try_get(0).map_err(decode)?);
    }
    Ok(out)
}

/// Check the write access on the transfers, then return their moves that are not draft, done or
/// cancelled.
pub(crate) async fn open_moves(records: &Records, tx: &mut Transaction) -> Result<Vec<i64>> {
    records.check_access(tx, Operation::Write).await?;
    tx.flush().await?;
    let mut out = Vec::new();
    for row in tx
        .query(
            "SELECT id FROM stock_move WHERE picking_id = ANY($1) AND is_scrap IS NOT TRUE \
               AND state NOT IN ('draft', 'done', 'cancel') ORDER BY id",
            &[&records.ids()],
        )
        .await?
    {
        out.push(row.try_get(0).map_err(decode)?);
    }
    Ok(out)
}

#[methods(StockPicking, addon = "stock")]
impl StockPicking {
    #[method]
    #[rpc(effect = write)]
    pub async fn do_unreserve(this: &RecordSet<StockPicking>, tx: &mut Transaction) -> Result<()> {
        let records = this.as_records();
        records.check_access(tx, Operation::Write).await?;
        tx.flush().await?;
        let moves = picking_moves(tx, &records.ids()).await?;
        unreserve_moves(records, tx, &moves).await
    }
}
