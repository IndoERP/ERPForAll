// SPDX-License-Identifier: MIT OR Apache-2.0
//! Make-to-stock confirmation of moves and transfers, with the early reservation it triggers, and
//! the defaults a new move takes from its transfer.
//!
//! Observed in the reference (320-328, 1773-1866`,
//! ): a move added to a transfer takes the
//! transfer's source, destination and operation type; confirming a draft move makes it `waiting`
//! when it has origin moves and `confirmed` otherwise, stamps today's reservation date when its
//! operation type reserves at confirmation, and reserves at once the moves that bypass reservation,
//! whose type reserves at confirmation, whose reservation date has come, or that return earlier
//! moves. Check availability confirms draft transfers first. Adopted: those rules. Modified: one
//! statement per company confirms its draft moves; the early reservation is one reservation call
//! for all of them, with reservation stamps and eligibility using each company's transaction day;
//! a transfer without a draft move to confirm is refused by name (the reference
//! does nothing).
//!
//! Not here yet, each refused by name or listed: make-to-order and make-to-stock-else-order moves
//! need the procurement engine (refused); merging moves of the same key and
//! negative demands turned into returns; assigning transfers to moves without one; the zero-demand
//! confirmation dialog; the replenishment trigger after confirmation.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use erp_addon_base::company_calendar::business_days;
use erp_sdk::Operation;
use erp_sdk::prelude::*;

use crate::error::StockError;
use crate::move_counters::{cell_id, lock_move_roots, notify_pickings};
use crate::picking::StockPicking;
use crate::reservation::{assign_moves, open_moves, picking_moves};
use crate::stock_move::StockMove;

/// What a new move takes from its transfer when it does not name it.
const PICKING_DEFAULTS: &str = "SELECT id, company_id, location_id, location_dest_id, \
       picking_type_id, partner_id FROM stock_picking WHERE id = ANY($1)";

/// Confirm the draft moves of `$1`: `waiting` behind origin moves, else `confirmed`; today's date
/// `$2` as reservation date where the operation type reserves at confirmation.
const CONFIRM: &str = "UPDATE stock_move m SET \
       state = CASE WHEN EXISTS (SELECT 1 FROM stock_move_move_rel o WHERE o.move_dest_id = m.id) \
                    THEN 'waiting' ELSE 'confirmed' END, \
       reservation_date = CASE WHEN t.reservation_method = 'at_confirm' \
                               THEN $2::text::date ELSE m.reservation_date END \
     FROM stock_move s \
     LEFT JOIN stock_picking p ON p.id = s.picking_id \
     LEFT JOIN stock_picking_type t ON t.id = COALESCE(s.picking_type_id, p.picking_type_id) \
     WHERE s.id = m.id AND m.id = ANY($1) AND m.state = 'draft' \
     RETURNING m.id";

/// The confirmed moves of `$1` that reserve at confirmation: returns,
/// moves that bypass reservation, moves whose type reserves at confirmation and moves whose
/// reservation date `$2` has come.
const ASSIGN_AT_CONFIRM: &str = "SELECT m.id FROM stock_move m \
     JOIN stock_location l ON l.id = m.location_id \
     JOIN product_product pp ON pp.id = m.product_id \
     JOIN product_template pt ON pt.id = pp.product_tmpl_id \
     LEFT JOIN stock_picking p ON p.id = m.picking_id \
     LEFT JOIN stock_picking_type t ON t.id = COALESCE(m.picking_type_id, p.picking_type_id) \
     WHERE m.id = ANY($1) \
       AND (m.state IN ('confirmed', 'partially_available') OR m.origin_returned_move_id IS NOT NULL) \
       AND (m.origin_returned_move_id IS NOT NULL \
         OR l.usage IN ('supplier', 'customer', 'inventory', 'production') OR NOT pt.is_storable \
         OR t.reservation_method = 'at_confirm' \
         OR m.reservation_date <= $2::text::date) \
     ORDER BY m.id";

fn decode(e: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::db("A stock move or transfer could not be read for confirmation.").with_source(e)
}

/// Fill the transfer's company, source, destination, operation type and contact into the values
/// of each new move that names a transfer (or has one as context default) and not the field.
/// One read for all transfers of the call.
///
/// The staged ORM values are flushed first, so a transfer whose header is changed in the same
/// save gives its new values.
///
/// # Errors
///
/// The database error of the flush or the read.
pub(crate) async fn fill_from_pickings(
    records: &Records,
    tx: &mut Transaction,
    vals_list: &mut [RawVals],
) -> Result<()> {
    let default_picking = match records.env().context().get("default_picking_id") {
        Some(Value::Int(id)) => Some(id),
        _ => None,
    };
    let picking_of = |vals: &RawVals| match vals.get("picking_id") {
        Some(cell) => cell_id(cell),
        None => default_picking,
    };
    let pickings: Vec<i64> = vals_list
        .iter()
        .filter_map(picking_of)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if pickings.is_empty() {
        return Ok(());
    }
    // a header change staged in the same save (values before the x2many commands) is read here
    tx.flush().await?;
    let mut rows: HashMap<i64, [Option<i64>; 5]> = HashMap::new();
    for row in tx.query(PICKING_DEFAULTS, &[&pickings]).await? {
        rows.insert(
            row.try_get(0).map_err(decode)?,
            [
                row.try_get(1).map_err(decode)?,
                row.try_get(2).map_err(decode)?,
                row.try_get(3).map_err(decode)?,
                row.try_get(4).map_err(decode)?,
                row.try_get(5).map_err(decode)?,
            ],
        );
    }
    const FIELDS: [&str; 5] = [
        "company_id",
        "location_id",
        "location_dest_id",
        "picking_type_id",
        "partner_id",
    ];
    for vals in vals_list.iter_mut() {
        // a missing transfer is left to the foreign key
        let Some(values) = picking_of(vals).and_then(|id| rows.get(&id)) else {
            continue;
        };
        for (field, value) in FIELDS.into_iter().zip(values) {
            if let Some(id) = value
                && !vals.contains(field)
            {
                vals.insert(field.into(), Cell::Id(*id));
            }
        }
    }
    Ok(())
}

/// Moves that are not draft are left as
/// they are. Validation confirms without the reservation: it makes the one engine call of its unit.
///
/// # Errors
///
/// [`StockError::NeedsProcurement`] for a make-to-order move; company calendar admission and zone
/// errors; the reservation's errors; read and database errors.
pub(crate) async fn confirm_moves(
    records: &Records,
    tx: &mut Transaction,
    moves: &[i64],
    assign: bool,
) -> Result<()> {
    let mut visited = std::collections::BTreeSet::new();
    confirm_moves_inner(records, tx, moves, assign, &mut visited, 0).await
}

async fn confirm_moves_inner(
    records: &Records,
    tx: &mut Transaction,
    moves: &[i64],
    assign: bool,
    visited: &mut std::collections::BTreeSet<(i64, i64, i64)>,
    depth: usize,
) -> Result<()> {
    if moves.is_empty() {
        return Ok(());
    }
    tx.flush().await?;
    let roots = lock_move_roots(tx, moves, &[]).await?;
    let upstream =
        crate::procurement::procure_mto_moves(records, tx, moves, visited, depth).await?;
    if !upstream.is_empty() {
        Box::pin(confirm_moves_inner(
            records,
            tx,
            &upstream,
            assign,
            visited,
            depth + 1,
        ))
        .await?;
    }
    // The whole request is locked above, before partitioning by company. Resolve every calendar
    // before changing any move so a bad zone cannot leave an earlier company confirmed.
    let mut by_company: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    for row in tx
        .query(
            "SELECT id, company_id FROM stock_move \
             WHERE id = ANY($1) AND state = 'draft' ORDER BY id",
            &[&moves],
        )
        .await?
    {
        let id = row.try_get::<_, i64>(0).map_err(decode)?;
        let company = row.try_get::<_, i64>(1).map_err(decode)?;
        by_company.entry(company).or_default().push(id);
    }
    let companies: Vec<i64> = by_company.keys().copied().collect();
    let days = business_days(tx, records.env(), &companies).await?;
    let mut confirmed = Vec::new();
    let mut partitions = Vec::new();
    for (company, drafts) in by_company {
        let today = days
            .get(&company)
            .ok_or_else(|| Error::internal("A stock move's company calendar was not read."))?
            .date
            .to_string();
        let mut company_confirmed = Vec::new();
        for row in tx.query(CONFIRM, &[&drafts, &today]).await? {
            company_confirmed.push(row.try_get::<_, i64>(0).map_err(decode)?);
        }
        confirmed.extend(company_confirmed.iter().copied());
        partitions.push((company_confirmed, today));
    }
    if confirmed.is_empty() {
        return Ok(());
    }
    confirmed.sort_unstable();
    records
        .env()
        .records("stock.move")?
        .browse(confirmed.iter().copied())?
        .modified_external(tx, &["state", "reservation_date"])
        .await?;
    notify_pickings(records, tx, &roots).await?;
    if !assign {
        return Ok(());
    }
    let mut eligible = Vec::new();
    for (company_confirmed, today) in partitions {
        for row in tx
            .query(ASSIGN_AT_CONFIRM, &[&company_confirmed, &today])
            .await?
        {
            eligible.push(row.try_get::<_, i64>(0).map_err(decode)?);
        }
    }
    eligible.sort_unstable();
    assign_moves(records, tx, &eligible).await
}

/// The draft moves of the transfers `pickings`, ascending.
pub(crate) async fn draft_moves(tx: &mut Transaction, pickings: &[i64]) -> Result<Vec<i64>> {
    let mut out = Vec::new();
    for row in tx
        .query(
            "SELECT id FROM stock_move WHERE picking_id = ANY($1) \
               AND state = 'draft' ORDER BY id",
            &[&pickings],
        )
        .await?
    {
        out.push(row.try_get(0).map_err(decode)?);
    }
    Ok(out)
}

#[methods(StockPicking, addon = "stock")]
impl StockPicking {
    /// Transfers with
    /// no draft move are refused by name.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_confirm(
        this: &RecordSet<StockPicking>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let records = this.as_records();
        records.check_access(tx, Operation::Write).await?;
        tx.flush().await?;
        let moves = draft_moves(tx, &records.ids()).await?;
        if moves.is_empty() {
            return Err(StockError::NothingToConfirm.into());
        }
        let before = crate::state_history::before(tx, records).await?;
        confirm_moves(records, tx, &moves, true).await?;
        crate::state_history::report(tx, records, before).await
    }

    /// A transfer with no open move is refused by name.
    ///
    /// One lock step and one engine call (4): the transfers and all their
    /// moves are locked ascending before the confirmation, which reserves nothing itself; the one
    /// reservation call covers the confirmed drafts too.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_assign(this: &RecordSet<StockPicking>, tx: &mut Transaction) -> Result<()> {
        let records = this.as_records();
        records.check_access(tx, Operation::Write).await?;
        tx.flush().await?;
        let ids = records.ids();
        let all = picking_moves(tx, &ids).await?;
        lock_move_roots(tx, &all, &ids).await?;
        let before = crate::state_history::before(tx, records).await?;
        let drafts = draft_moves(tx, &ids).await?;
        confirm_moves(records, tx, &drafts, false).await?;
        let moves = open_moves(records, tx).await?;
        if moves.is_empty() {
            return Err(StockError::NothingToAssign.into());
        }
        assign_moves(records, tx, &moves).await?;
        crate::state_history::report(tx, records, before).await
    }
}

#[methods(StockMove, addon = "stock")]
impl StockMove {
    /// A product picked on a new move proposes its unit (on the form).
    #[onchange("product_id")]
    async fn _onchange_product_unit(
        this: &RecordSet<StockMove>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        for mv in this.as_records().iter() {
            let product = mv.related(tx, "product_id").await?;
            if product.is_empty() {
                continue;
            }
            let unit = product.cell(tx, "uom_id").await?;
            mv.write_base(tx, RawVals::new().set_cell("uom_id", unit))
                .await?;
        }
        Ok(OnchangeResult::none())
    }
}
