// SPDX-License-Identifier: MIT OR Apache-2.0
//! Done: validation of transfers, the done transition of their moves and lines, and the rule that
//! done work is fixed (D3, D6.7;.2 I4).
//!
//! Observed in the reference (1127-1206,
//! 1270-1364`;
//! ): Validate skips transfers that are done
//! already, refuses a transfer without moves and one whose moves carry no quantity, marks every
//! move picked when nothing is picked yet, cancels the moves without quantity whose demand is zero
//! (or every unpicked or empty move when the operation type never creates backorders), splits a
//! backorder for what is short otherwise, deletes zero lines, and for each remaining line releases
//! its reservation at the source, takes its quantity off the source and adds it at the destination;
//! the moves become done with the completion time and the transfer gets its done date. Adopted:
//! those rules. Modified:
//! - **one engine call** for the whole call (above the engine's bound, ascending chunks in one
//!   savepoint): the reservation releases of the done and deleted lines
//!   and the on-hand changes of the done lines are netted per quant key and applied in the engine's
//!   key order, after the transfers and then the moves are locked. A location that
//!   forbids negative stock refuses the call by name and nothing of it is written;
//! - the line deletions run through the line model with the settlement deferred to this unit
//!   ([`DeferSettle`]), so they add no engine call of their own;
//! - the done marks are three statements (lines, moves, transfers); the transfer counters and state
//!   follow from the move counter triggers;
//! - on a short direct receipt or delivery, Always creates the outstanding demand in a new transfer.
//!   Never finishes the available work. Ask opens a decision through the transfer form.
//!   Cancelling a move that feeds open moves remains refused ([`StockError::CancelChained`]).
//!
//! **Done work is fixed.** A done move, its lines and a done transfer keep their quantities,
//! product, locations, state and links for every caller, sudo included: a write naming a field
//! outside [`DONE_MOVE_WRITABLE`] or [`DONE_PICKING_WRITABLE`] is refused by name, also when it comes
//! as x2many commands; a move is not added to a done transfer (in the values or as the context
//! default); done moves and transfers are not deleted; a cancelled move and a cancelled transfer
//! are fixed the same way (a cancelled transfer allows [`CANCELLED_PICKING_WRITABLE`]), and may be
//! deleted. Cancel, validation and the batch actions write their own columns in SQL statements or
//! within the allow-lists, so none of them meets these refusals; and no create, write or context default sets
//! a move to done: validation is the only way there. Done lines were already refused by the line
//! statements (`move_lines`).
//!
//! A draft transfer is confirmed on the way and its moves without quantity get a line for their
//! demand; the early reservation of the confirmation is skipped, since that
//! line is moved at once. Those lines were never reserved, so at a location that forbids negative
//! stock they may not take stock reserved for other transfers: the engine holds their source keys to
//! the free quantity in the same call and the validation is refused by name with nothing changed
//! (3); elsewhere they take it, as over-reservation is legal there (O7).
//!
//! Not here yet: multi-transfer backorder choices, manual splitting, lots, the line picked flag (every line of a done move is done), the incoming date carried from the source
//! balance (the destination keeps its own or takes now), the re-floor of other reservations when a
//! validation drives free stock negative at an unflagged location, the
//! assignment of destination moves, packages consistency, the follower message on a cancelled
//! remainder.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use erp_sdk::Operation;
use erp_sdk::prelude::*;

use crate::confirm::{confirm_moves, draft_moves};
use crate::error::StockError;
use crate::move_counters::{cell_id, lock_move_roots, lock_pickings, notify_pickings};
use crate::move_lines::{DeferSettle, LINE_DERIVED_MOVE_FIELDS, reservation_changes, sync_moves};
use crate::picking::StockPicking;
use crate::quant_engine::{QuantChange, QuantKey, apply_quant_changes_in_chunks};
use crate::stock_move_line::StockMoveLine;
use crate::warehouse_setup::without_defaults;

/// The fields a done move may still change: descriptions, ordering and chain links, nothing that
/// moves stock. The one allow-list of done moves.
pub const DONE_MOVE_WRITABLE: [&str; 8] = [
    "description_picking_manual",
    "origin",
    "priority",
    "sequence",
    "date_deadline",
    "move_dest_ids",
    "move_orig_ids",
    "reference_ids",
];

/// The fields a done transfer may still change: notes, signature and bookkeeping, nothing that moves stock.
/// The one allow-list of done transfers.
pub const DONE_PICKING_WRITABLE: [&str; 8] = [
    "note",
    "origin",
    "user_id",
    "printed",
    "is_locked",
    "priority",
    "signed_by",
    "signature",
];

/// The fields a cancelled transfer may still change: those of a done one, and the batch link that
/// only the batch actions write (`crate::batch::refuse_link`), so a cancelled batch lets its
/// cancelled transfers go. A cancelled move keeps [`DONE_MOVE_WRITABLE`].
pub const CANCELLED_PICKING_WRITABLE: [&str; 7] = [
    "note",
    "origin",
    "user_id",
    "printed",
    "is_locked",
    "priority",
    "batch_id",
];

/// The transfers of `$1` with their state, move count and backorder policy, locked first.
const READ_PICKINGS: &str = "SELECT p.id, p.state, p.move_count, \
       t.create_backorder \
     FROM stock_picking p LEFT JOIN stock_picking_type t ON t.id = p.picking_type_id \
     WHERE p.id = ANY($1) ORDER BY p.id";

/// The open moves of the transfers `$1`, with whether an open move takes from them.
const READ_MOVES: &str = "SELECT m.id, m.picking_id, m.picked, m.quantity, m.product_uom_qty, \
       m.product_id, EXISTS (SELECT 1 FROM stock_move_move_rel r \
         JOIN stock_move d ON d.id = r.move_dest_id \
         WHERE r.move_orig_id = m.id AND d.state NOT IN ('done', 'cancel')) \
     FROM stock_move m WHERE m.picking_id = ANY($1) AND m.state NOT IN ('done', 'cancel') \
     ORDER BY m.id";

/// The open, active lines of the moves `$1`.
const READ_LINES: &str = "SELECT id, move_id, quantity, done_at IS NULL FROM stock_move_line \
     WHERE move_id = ANY($1) AND membership = 'open' AND active ORDER BY id";

/// What the done lines `$1` of storable products move: source key, destination, result package and
/// quantity in the product unit.
const LINE_EFFECTS: &str = "SELECT l.company_id, l.product_id, l.location_id, l.lot_id, \
       l.package_id, l.owner_id, l.location_dest_id, l.result_package_id, l.quantity_product_uom, \
       l.id \
     FROM stock_move_line l \
     JOIN product_product pp ON pp.id = l.product_id \
     JOIN product_template pt ON pt.id = pp.product_tmpl_id \
     WHERE l.id = ANY($1) AND pt.is_storable AND l.quantity_product_uom <> 0";

// The done marks keep a done time a row already has (the done-time guard refuses changing one) and
// stamp the write audit with the validating user `$2`.
const LINES_DONE: &str = "UPDATE stock_move_line SET membership = 'done', picked = true, \
       done_at = COALESCE(done_at, (now() AT TIME ZONE 'UTC')), \
       write_uid = $2, write_date = (now() AT TIME ZONE 'UTC') WHERE id = ANY($1)";
/// The moves validation cancels (no quantity, no demand); the cancel action has its own statement.
const MOVES_CANCEL_EMPTY: &str = "UPDATE stock_move SET state = 'cancel', \
       write_uid = $2, write_date = (now() AT TIME ZONE 'UTC') WHERE id = ANY($1)";
const MOVES_DONE: &str = "UPDATE stock_move SET state = 'done', picked = true, \
       date = (now() AT TIME ZONE 'UTC'), done_at = COALESCE(done_at, (now() AT TIME ZONE 'UTC')), \
       write_uid = $2, write_date = (now() AT TIME ZONE 'UTC') WHERE id = ANY($1)";
const PICKINGS_DONE: &str = "UPDATE stock_picking SET date_done = (now() AT TIME ZONE 'UTC'), \
       priority = '0', write_uid = $2, write_date = (now() AT TIME ZONE 'UTC') WHERE id = ANY($1)";

const DONE_MOVES: &str = "SELECT count(*) FROM stock_move WHERE id = ANY($1) AND state = 'done'";
const DONE_PICKINGS: &str =
    "SELECT count(*) FROM stock_picking WHERE id = ANY($1) AND state = 'done'";
/// The final state among the moves or transfers `$1`, `done` before `cancel`.
const FINAL_MOVE: &str = "SELECT state FROM stock_move WHERE id = ANY($1) \
     AND state IN ('done', 'cancel') ORDER BY state DESC LIMIT 1";
const FINAL_PICKING: &str = "SELECT state FROM stock_picking WHERE id = ANY($1) \
     AND state IN ('done', 'cancel') ORDER BY state DESC LIMIT 1";

fn decode(e: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::db("A transfer, stock move or move line could not be read for validation.")
        .with_source(e)
}

fn label(tx: &Transaction, model: &str, field: &str) -> Result<String> {
    tx.registry()
        .model(model)
        .and_then(|spec| spec.field(field))
        .map(|spec| spec.string.clone())
        .ok_or_else(|| Error::internal(format!("{model} has no field '{field}'")))
}

async fn count(tx: &mut Transaction, sql: &str, ids: &[i64]) -> Result<i64> {
    tx.query(sql, &[&ids])
        .await?
        .first()
        .ok_or_else(|| Error::internal("A count returned no row."))?
        .try_get(0)
        .map_err(decode)
}

/// The final state (`done` before `cancel`) among the rows `ids` of `sql`'s table, if any.
pub(crate) async fn final_state(
    tx: &mut Transaction,
    sql: &str,
    ids: &[i64],
) -> Result<Option<String>> {
    match tx.query(sql, &[&ids]).await?.first() {
        Some(row) => row.try_get(0).map(Some).map_err(decode),
        None => Ok(None),
    }
}

/// The refusal of `field` (a field name of `model`) on a record of `what` in the final `state`.
pub(crate) fn fixed(
    tx: &Transaction,
    model: &str,
    what: &'static str,
    state: &str,
    field: &str,
) -> Result<Error> {
    let field = label(tx, model, field)?;
    Ok(if state == "done" {
        StockError::DoneRecordFixed { what, field }
    } else {
        StockError::CancelledRecordFixed { what, field }
    }
    .into())
}

/// The first field `vals` names (as a value or as x2many commands) outside `writable`.
pub(crate) fn outside<'a>(vals: &'a RawVals, writable: &[&str]) -> Option<&'a str> {
    vals.iter()
        .map(|(name, _)| name)
        .chain(vals.iter_commands().map(|(name, _)| name))
        .find(|name| !writable.contains(name))
}

/// The caller-named transfers `ids` must be readable before a refusal tells done from cancelled.
async fn readable_transfers(records: &Records, tx: &mut Transaction, ids: &[i64]) -> Result<()> {
    records
        .env()
        .records("stock.picking")?
        .browse(ids.iter().copied())?
        .check_access(tx, Operation::Read)
        .await
}

/// Refuse a move create that adds a move to a done or cancelled transfer (the state itself is refused for every
/// caller by the move guards). Runs after the create access check.
pub(crate) async fn check_move_create(
    records: &Records,
    tx: &mut Transaction,
    vals_list: &[RawVals],
) -> Result<()> {
    let context = records.env().context();
    let default_picking = match context.get("default_picking_id") {
        Some(Value::Int(id)) => Some(id),
        _ => None,
    };
    let pickings: Vec<i64> = vals_list
        .iter()
        .filter_map(|vals| match vals.get("picking_id") {
            Some(cell) => cell_id(cell),
            None => default_picking,
        })
        .collect();
    if pickings.is_empty() {
        return Ok(());
    }
    if let Some(state) = final_state(tx, FINAL_PICKING, &pickings).await? {
        records.check_access(tx, Operation::Create).await?;
        readable_transfers(records, tx, &pickings).await?;
        return Err(fixed(tx, "stock.picking", "transfer", &state, "move_ids")?);
    }
    Ok(())
}

/// Refuse a move write that moves a move into a done or cancelled transfer, or changes a done or
/// cancelled move outside [`DONE_MOVE_WRITABLE`]. Runs after the write access check.
pub(crate) async fn check_move_write(
    records: &Records,
    tx: &mut Transaction,
    vals: &RawVals,
) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    if let Some(picking) = vals.get("picking_id").and_then(cell_id)
        && let Some(state) = final_state(tx, FINAL_PICKING, &[picking]).await?
    {
        records.check_access(tx, Operation::Write).await?;
        readable_transfers(records, tx, &[picking]).await?;
        return Err(fixed(tx, "stock.picking", "transfer", &state, "move_ids")?);
    }
    let Some(field) = outside(vals, &DONE_MOVE_WRITABLE) else {
        return Ok(());
    };
    tx.flush().await?;
    if let Some(state) = final_state(tx, FINAL_MOVE, &records.ids()).await? {
        records.check_access(tx, Operation::Write).await?;
        return Err(fixed(tx, "stock.move", "stock move", &state, field)?);
    }
    Ok(())
}

/// Refuse a write of a done transfer outside [`DONE_PICKING_WRITABLE`] and of a cancelled one
/// outside [`CANCELLED_PICKING_WRITABLE`]. Runs after the write access check.
pub(crate) async fn check_picking_write(
    records: &Records,
    tx: &mut Transaction,
    vals: &RawVals,
) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let Some(field) = outside(vals, &DONE_PICKING_WRITABLE) else {
        return Ok(());
    };
    tx.flush().await?;
    let field = match final_state(tx, FINAL_PICKING, &records.ids()).await? {
        None => return Ok(()),
        Some(state) if state == "done" => (state, field),
        // the batch link is the one field a cancelled transfer allows beyond a done one
        Some(state) => match outside(vals, &CANCELLED_PICKING_WRITABLE) {
            Some(field) => (state, field),
            None => return Ok(()),
        },
    };
    records.check_access(tx, Operation::Write).await?;
    Err(fixed(tx, "stock.picking", "transfer", &field.0, field.1)?)
}

/// Refuse deleting done moves (`model` `stock.move`) or done transfers (`stock.picking`). Runs
/// after the unlink access check.
pub(crate) async fn check_unlink(
    records: &Records,
    tx: &mut Transaction,
    model: &str,
) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let (sql, what) = if model == "stock.picking" {
        (DONE_PICKINGS, "transfer")
    } else {
        (DONE_MOVES, "stock move")
    };
    tx.flush().await?;
    if count(tx, sql, &records.ids()).await? > 0 {
        records.check_access(tx, Operation::Unlink).await?;
        return Err(StockError::DoneDelete { what }.into());
    }
    Ok(())
}

/// The zero-quantity open lines of the moves `$1`: validation deletes them anyway, and one of them
/// could hold the open-line key the demand line takes.
const ZERO_LINES: &str = "SELECT id FROM stock_move_line \
     WHERE move_id = ANY($1) AND membership = 'open' AND quantity = 0 ORDER BY id";

/// The moves of `$1` without quantity and with a positive demand.
const UNFILLED: &str = "SELECT id, product_uom_qty FROM stock_move \
     WHERE id = ANY($1) AND quantity = 0 AND product_uom_qty > 0 \
       AND state NOT IN ('done', 'cancel') ORDER BY id";

/// Give each of `moves` without quantity one line for its demand at its own locations, inserted
/// through the line model without a reservation (the validating unit moves it at once) and without
/// the caller's `default_*` context, and set the moves' quantities. The moves' zero-quantity lines
/// are deleted first. Returns the new lines.
async fn fill_demand(
    records: &Records,
    tx: &mut Transaction,
    moves: &[i64],
) -> Result<BTreeSet<i64>> {
    let mut vals_list = Vec::new();
    let mut filled = Vec::new();
    for row in tx.query(UNFILLED, &[&moves]).await? {
        let id: i64 = row.try_get(0).map_err(decode)?;
        let demand: Decimal = row.try_get(1).map_err(decode)?;
        filled.push(id);
        vals_list.push(
            RawVals::new()
                .set_cell("move_id", Cell::Id(id))
                .set_cell("quantity", Cell::Decimal(demand)),
        );
    }
    if vals_list.is_empty() {
        return Ok(BTreeSet::new());
    }
    let mut zero = Vec::new();
    for row in tx.query(ZERO_LINES, &[&filled]).await? {
        zero.push(row.try_get::<_, i64>(0).map_err(decode)?);
    }
    let model = without_defaults(records.env()).records("stock.move.line")?;
    let zero = model.browse(zero)?;
    tx.extension::<DeferSettle>().active = true;
    let deleted = if zero.is_empty() {
        Ok(())
    } else {
        zero.unlink(tx).await
    };
    let result = match deleted {
        Ok(()) => model.create(tx, vals_list).await,
        Err(error) => Err(error),
    };
    tx.extension::<DeferSettle>().active = false;
    let created = result?.ids();
    tx.flush().await?;
    sync_moves(tx, &filled).await?;
    Ok(created.into_iter().collect())
}

/// One open move as validation reads it.
struct OpenMove {
    id: i64,
    picking: i64,
    picked: bool,
    quantity: Decimal,
    demand: Decimal,
    product: i64,
    feeds_open: bool,
}

async fn product_name(tx: &mut Transaction, product: i64) -> Result<String> {
    match tx
        .env()
        .records("product.product")?
        .browse(vec![product])?
        .cell(tx, "display_name")
        .await?
    {
        Cell::Str(name) => Ok(name.to_string()),
        _ => Err(Error::internal(format!(
            "product {product} has no display name"
        ))),
    }
}

/// The caller
/// checks the write right on `records` before anything of them is read.
///
/// # Errors
///
/// The refusals of [`StockError`] named in the module documentation; the engine's refusals
/// ([`crate::QuantError::NegativeStockForbidden`]); a retryable `40001` when a transfer changed
/// since the transaction's snapshot; read and database errors.
async fn validate(records: &Records, tx: &mut Transaction) -> Result<Vec<i64>> {
    tx.flush().await?;
    let ids = records.ids();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let origins = crate::returns::completion_origins(records, tx, &ids).await?;
    let mut picking_roots = ids.clone();
    picking_roots.extend_from_slice(&origins);
    lock_pickings(tx, &picking_roots).await?;
    let mut todo_pickings = Vec::new();
    let mut drafts = Vec::new();
    let mut policies = HashMap::new();
    for row in tx.query(READ_PICKINGS, &[&ids]).await? {
        let id: i64 = row.try_get(0).map_err(decode)?;
        let state: String = row.try_get(1).map_err(decode)?;
        let moves: i32 = row.try_get(2).map_err(decode)?;
        match state.as_str() {
            "done" => continue,
            "cancel" => return Err(StockError::ValidateCancelled.into()),
            _ if moves == 0 => return Err(StockError::ValidateEmpty.into()),
            "draft" => drafts.push(id),
            _ => {}
        }
        policies.insert(id, row.try_get::<_, String>(3).map_err(decode)?);
        todo_pickings.push(id);
    }
    if todo_pickings.is_empty() {
        return Err(StockError::AlreadyDone.into());
    }
    // a draft transfer is confirmed on the way (without the early reservation: this unit makes one
    // engine call), and its moves without quantity take their demand
    let fresh: BTreeSet<i64> = if drafts.is_empty() {
        BTreeSet::new()
    } else {
        let draft = draft_moves(tx, &drafts).await?;
        confirm_moves(records, tx, &draft, false).await?;
        fill_demand(records, tx, &draft).await?
    };
    let mut moves = Vec::new();
    for row in tx.query(READ_MOVES, &[&todo_pickings]).await? {
        moves.push(OpenMove {
            id: row.try_get(0).map_err(decode)?,
            picking: row.try_get(1).map_err(decode)?,
            picked: row.try_get(2).map_err(decode)?,
            quantity: row.try_get(3).map_err(decode)?,
            demand: row.try_get(4).map_err(decode)?,
            product: row.try_get(5).map_err(decode)?,
            feeds_open: row.try_get(6).map_err(decode)?,
        });
    }
    let move_ids: Vec<i64> = moves.iter().map(|m| m.id).collect();
    let roots = lock_move_roots(tx, &move_ids, &todo_pickings).await?;

    // sanity and the picked default (`:1270-1284`), per transfer
    for picking in &todo_pickings {
        let of: Vec<&mut OpenMove> = moves.iter_mut().filter(|m| m.picking == *picking).collect();
        let has_pick = of.iter().any(|m| m.picked);
        if of
            .iter()
            .filter(|m| !has_pick || m.picked)
            .all(|m| m.quantity.is_zero())
        {
            return Err(StockError::ValidateNoQuantity.into());
        }
        if !has_pick {
            for m in of {
                m.picked = true;
            }
        }
    }
    // what is done, what is cancelled, what would need a backorder
    let mut done_moves = Vec::new();
    let mut cancel_moves = Vec::new();
    let mut shortages = BTreeMap::new();
    let choice = tx
        .extension::<crate::backorder_choice::BackorderDecision>()
        .0;
    let mut applied_choice = false;
    for m in &moves {
        let policy = policies
            .get(&m.picking)
            .ok_or_else(|| Error::invariant("Missing transfer backorder policy."))?;
        let todo = m.picked && m.quantity > Decimal::ZERO;
        let short = if todo {
            m.quantity < m.demand
        } else {
            m.demand > Decimal::ZERO
        };
        let explicit = choice
            .filter(|(id, _)| *id == m.picking && policy == "ask" && short)
            .map(|(_, keep)| keep);
        applied_choice |= explicit.is_some();
        let never = policy == "never" || explicit == Some(false);
        if todo && (m.quantity >= m.demand || never) {
            done_moves.push(m.id);
        } else if !todo && (m.demand.is_zero() || never) {
            if m.feeds_open {
                return Err(StockError::CancelChained.into());
            }
            cancel_moves.push(m.id);
        } else if policy == "always" || explicit == Some(true) {
            let completed = if todo { m.quantity } else { Decimal::ZERO };
            let remaining = m.demand.checked_sub(completed).ok_or_else(|| {
                Error::validation(
                    "The remaining stock demand is outside the supported quantity range.",
                )
            })?;
            shortages
                .entry(m.picking)
                .or_insert_with(Vec::new)
                .push(crate::backorder::ShortMove {
                    id: m.id,
                    completed,
                    remaining,
                });
            if todo {
                done_moves.push(m.id);
            } else {
                cancel_moves.push(m.id);
            }
        } else {
            return Err(StockError::BackorderNotAvailable {
                product: product_name(tx, m.product).await?,
            }
            .into());
        }
    }

    if choice.is_some() && !applied_choice {
        return Err(Error::user(
            "This transfer no longer needs an Ask backorder decision. Reopen the transfer to validate its current quantities.",
        ));
    }
    let backorders = crate::backorder::split(tx, records, &shortages).await?;
    let mut deleted = Vec::new();
    let mut done_lines = Vec::new();
    let mut original_lines = Vec::new();
    let cancelled: BTreeSet<i64> = cancel_moves.iter().copied().collect();
    for row in tx.query(READ_LINES, &[&move_ids]).await? {
        let id: i64 = row.try_get(0).map_err(decode)?;
        let move_id: i64 = row.try_get(1).map_err(decode)?;
        let quantity: Decimal = row.try_get(2).map_err(decode)?;
        if cancelled.contains(&move_id) || quantity.is_zero() {
            deleted.push(id);
        } else {
            done_lines.push(id);
            if row.try_get::<_, bool>(3).map_err(decode)? {
                original_lines.push(id);
            }
        }
    }

    // Capture reservation releases BEFORE updating owner_id or lot_id so released keys match
    // the exact QuantKeys originally reserved (preventing ReservationUnderflow).
    let mut released = HashMap::new();
    // the demand lines of a draft were never reserved
    let mut touched = deleted.clone();
    touched.extend(done_lines.iter().filter(|id| !fresh.contains(id)));
    reservation_changes(tx, &touched, Decimal::NEGATIVE_ONE, &mut released).await?;

    // Propagate transfer consignment owner to lines that have no explicit owner set.
    if !done_lines.is_empty() {
        let updated_owners = tx
            .query(
                "UPDATE stock_move_line l SET owner_id = p.owner_id \
                 FROM stock_picking p \
                 WHERE l.id = ANY($1) AND l.picking_id = p.id \
                   AND l.owner_id IS NULL AND p.owner_id IS NOT NULL \
                 RETURNING l.id::int8",
                &[&done_lines],
            )
            .await?;
        if !updated_owners.is_empty() {
            let owner_line_ids: Vec<i64> = updated_owners
                .iter()
                .map(|r| r.try_get::<_, i64>(0).map_err(decode))
                .collect::<Result<_>>()?;
            records
                .env()
                .records("stock.move.line")?
                .browse(owner_line_ids)?
                .modified_external(tx, &["owner_id"])
                .await?;
        }
    }
    resolve_and_check_lots(records, tx, &done_lines).await?;
    crate::returns::guard_completion(records, tx, &origins, &done_lines).await?;

    // one engine call: the released reservations and the on-hand changes, netted per key
    let mut nets: HashMap<QuantKey, (Decimal, Decimal)> = released
        .into_iter()
        .map(|(key, reserved)| (key, (Decimal::ZERO, reserved)))
        .collect();
    let mut floored = HashSet::new();
    for row in tx.query(LINE_EFFECTS, &[&done_lines]).await? {
        let company: i64 = row.try_get(0).map_err(decode)?;
        let product: i64 = row.try_get(1).map_err(decode)?;
        let lot: Option<i64> = row.try_get(3).map_err(decode)?;
        let owner: Option<i64> = row.try_get(5).map_err(decode)?;
        let quantity: Decimal = row.try_get(8).map_err(decode)?;
        let source = QuantKey::new(company, product, row.try_get(2).map_err(decode)?)
            .lot_id(lot)
            .package_id(row.try_get(4).map_err(decode)?)
            .owner_id(owner);
        let dest = QuantKey::new(company, product, row.try_get(6).map_err(decode)?)
            .lot_id(lot)
            .package_id(row.try_get(7).map_err(decode)?)
            .owner_id(owner);
        if fresh.contains(&row.try_get::<_, i64>(9).map_err(decode)?) {
            floored.insert(source);
        }
        nets.entry(source).or_default().0 -= quantity;
        nets.entry(dest).or_default().0 += quantity;
    }
    // the demand lines of a draft take stock that was never reserved: held to the free quantity
    let changes: Vec<QuantChange> = nets
        .into_iter()
        .filter(|(_, (quantity, reserved))| !(quantity.is_zero() && reserved.is_zero()))
        .map(|(key, (quantity, reserved))| {
            let change = QuantChange::new(key, quantity, reserved);
            if floored.contains(&key) {
                change.as_floored()
            } else {
                change
            }
        })
        .collect();
    // in ascending key chunks under the engine's bound, so a large validation is not refused
    apply_quant_changes_in_chunks(tx, &changes).await?;

    let env = records.env();
    let uid = env.uid();
    if !deleted.is_empty() {
        let lines = env.records("stock.move.line")?.browse(deleted)?;
        tx.extension::<DeferSettle>().active = true;
        let result = lines.unlink(tx).await;
        tx.extension::<DeferSettle>().active = false;
        result?;
    }
    if !done_lines.is_empty() {
        tx.execute(LINES_DONE, &[&done_lines, &uid]).await?;
        env.records("stock.move.line")?
            .browse(done_lines)?
            .modified_external(tx, &["membership", "picked", "done_at", "write_date"])
            .await?;
    }
    sync_moves(tx, &move_ids).await?;
    if !cancel_moves.is_empty() {
        tx.execute(MOVES_CANCEL_EMPTY, &[&cancel_moves, &uid])
            .await?;
    }
    if !done_moves.is_empty() {
        tx.execute(MOVES_DONE, &[&done_moves, &uid]).await?;
    }
    tx.execute(PICKINGS_DONE, &[&todo_pickings, &uid]).await?;
    // `write_uid` is not notified: an external change of a relational field is not supported yet
    let mut fields: Vec<&str> = LINE_DERIVED_MOVE_FIELDS.to_vec();
    fields.extend(["picked", "date", "done_at", "write_date"]);
    env.records("stock.move")?
        .browse(move_ids)?
        .modified_external(tx, &fields)
        .await?;
    env.records("stock.picking")?
        .browse(todo_pickings)?
        .modified_external(tx, &["date_done", "priority", "write_date"])
        .await?;
    notify_pickings(records, tx, &roots).await?;
    confirm_moves(records, tx, &backorders, true).await?;
    crate::procurement::propagate_push_and_downstream(records, tx, &done_moves).await?;
    Ok(original_lines)
}

/// Resolve `lot_name` into `stock.lot` records and verify lot/serial tracking rules on lines
/// entering the done ledger.
async fn resolve_and_check_lots(
    records: &Records,
    tx: &mut Transaction,
    done_lines: &[i64],
) -> Result<()> {
    if done_lines.is_empty() {
        return Ok(());
    }
    let rows = tx
        .query(
            "SELECT l.id::int8, l.company_id::int8, l.product_id::int8, l.lot_id::int8, \
                    l.lot_name, l.quantity_product_uom, pt.is_storable, pt.tracking, \
                    COALESCE(t.use_create_lots, true), ls.usage, ld.usage \
             FROM stock_move_line l \
             JOIN product_product pp ON pp.id = l.product_id \
             JOIN product_template pt ON pt.id = pp.product_tmpl_id \
             JOIN stock_location ls ON ls.id = l.location_id \
             JOIN stock_location ld ON ld.id = l.location_dest_id \
             LEFT JOIN stock_picking p ON p.id = l.picking_id \
             LEFT JOIN stock_picking_type t ON t.id = p.picking_type_id \
             WHERE l.id = ANY($1) \
             ORDER BY l.id",
            &[&done_lines],
        )
        .await?;
    let lot_model = records.env().records("stock.lot")?;
    let mut seen_serials = BTreeSet::<(i64, i64, i64)>::new();
    let mut updated_lot_lines = Vec::new();
    for row in rows {
        let line_id: i64 = row.try_get(0).map_err(decode)?;
        let company_id: i64 = row.try_get(1).map_err(decode)?;
        let product_id: i64 = row.try_get(2).map_err(decode)?;
        let mut lot_id: Option<i64> = row.try_get(3).map_err(decode)?;
        let lot_name: Option<String> = row.try_get(4).map_err(decode)?;
        let qty_product_uom: Decimal = row.try_get(5).map_err(decode)?;
        let _is_storable: bool = row.try_get(6).map_err(decode)?;
        let tracking: Option<String> = row.try_get(7).map_err(decode)?;
        let use_create_lots: bool = row.try_get(8).map_err(decode)?;
        let src_usage: String = row.try_get(9).map_err(decode)?;
        let dst_usage: String = row.try_get(10).map_err(decode)?;

        if lot_id.is_none()
            && let Some(raw_name) = lot_name.as_deref().map(str::trim).filter(|s| !s.is_empty())
        {
            let existing = tx
                .query(
                    "SELECT id::int8 FROM stock_lot \
                     WHERE product_id = $1 AND name = $2 \
                       AND (company_id = $3 OR company_id IS NULL) \
                     ORDER BY company_id NULLS LAST, id LIMIT 1",
                    &[&product_id, &raw_name, &company_id],
                )
                .await?;
            let resolved = if let Some(found) = existing.first() {
                found.try_get::<_, i64>(0).map_err(decode)?
            } else {
                if !use_create_lots {
                    let name = product_name(tx, product_id).await?;
                    return Err(Error::user(format!(
                        "Cannot create new lot/serial number '{raw_name}' for product '{name}': \
                         this operation type only allows existing lots."
                    )));
                }
                let mut vals = RawVals::new();
                vals.insert("name".into(), Cell::Str(raw_name.into()));
                vals.insert("product_id".into(), Cell::Id(product_id));
                vals.insert("company_id".into(), Cell::Id(company_id));
                let created = lot_model.create(tx, vec![vals]).await?;
                tx.flush().await?;
                created
                    .ids()
                    .into_iter()
                    .next()
                    .ok_or_else(|| Error::invariant("Created stock.lot returned no id."))?
            };
            tx.execute(
                "UPDATE stock_move_line SET lot_id = $1 WHERE id = $2",
                &[&resolved, &line_id],
            )
            .await?;
            updated_lot_lines.push(line_id);
            lot_id = Some(resolved);
        }

        if !qty_product_uom.is_zero() {
            match tracking.as_deref() {
                Some("lot") => {
                    if lot_id.is_none() {
                        let name = product_name(tx, product_id).await?;
                        return Err(Error::user(format!(
                            "You need to supply a Lot/Serial Number for product '{name}'."
                        )));
                    }
                }
                Some("serial") => {
                    let Some(sid) = lot_id else {
                        let name = product_name(tx, product_id).await?;
                        return Err(Error::user(format!(
                            "You need to supply a Serial Number for product '{name}'."
                        )));
                    };
                    if qty_product_uom != Decimal::ONE {
                        let name = product_name(tx, product_id).await?;
                        return Err(Error::user(format!(
                            "Serial-tracked product '{name}' must be moved with quantity 1 per line (got {qty_product_uom})."
                        )));
                    }
                    if !seen_serials.insert((company_id, product_id, sid)) {
                        let name = product_name(tx, product_id).await?;
                        return Err(Error::user(format!(
                            "Serial number cannot appear on multiple lines in the same validation for product '{name}'."
                        )));
                    }
                    if dst_usage == "internal" && src_usage != "internal" {
                        let existing_qty: Decimal = tx
                            .query(
                                "SELECT COALESCE(SUM(q.quantity), 0) FROM stock_quant q \
                                 JOIN stock_location l ON l.id = q.location_id \
                                 WHERE q.company_id = $1 AND q.product_id = $2 AND q.lot_id = $3 \
                                   AND l.usage = 'internal' AND q.quantity > 0",
                                &[&company_id, &product_id, &sid],
                            )
                            .await?
                            .first()
                            .ok_or_else(|| {
                                Error::internal("Serial uniqueness check returned no row.")
                            })?
                            .try_get(0)
                            .map_err(decode)?;
                        if existing_qty > Decimal::ZERO {
                            let name = product_name(tx, product_id).await?;
                            return Err(Error::user(format!(
                                "Serial number is already in stock for product '{name}' and cannot be received twice."
                            )));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    if !updated_lot_lines.is_empty() {
        records
            .env()
            .records("stock.move.line")?
            .browse(updated_lot_lines)?
            .modified_external(tx, &["lot_id"])
            .await?;
    }
    Ok(())
}

#[methods(StockPicking, addon = "stock")]
impl StockPicking {
    /// Done transfers among them are
    /// skipped; see [`validate`] for the refusals.
    #[method]
    #[rpc(effect = write)]
    pub async fn button_validate(
        this: &RecordSet<StockPicking>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let records = this.as_records().clone();
        records.check_access(tx, Operation::Write).await?;
        tx.savepoint(async move |tx| {
            let before = crate::state_history::before(tx, &records).await?;
            let original = validate(&records, tx).await?;
            crate::state_history::report(tx, &records, before).await?;
            if !original.is_empty() {
                records
                    .env()
                    .records("stock.move.line")?
                    .browse(original)?
                    .call::<(), ()>(tx, "_after_stock_completion", ())
                    .await?;
            }
            Ok(())
        })
        .await
    }
}

#[methods(StockMoveLine, addon = "stock")]
impl StockMoveLine {
    /// Runs under the validating caller in the same savepoint as the physical transfer. Only
    /// original lines just moved to done are included. A corrected or reopened line has its own
    /// original date and is outside this hook. Observers must establish any earlier lock class
    /// before validation and check their own source authority before reading or admitting facts.
    #[method]
    async fn _after_stock_completion(
        _this: &RecordSet<StockMoveLine>,
        _tx: &mut Transaction,
    ) -> Result<()> {
        Ok(())
    }
}
