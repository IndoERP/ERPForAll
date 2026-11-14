// SPDX-License-Identifier: MIT OR Apache-2.0
//! Bounded reservation policy and the dates changed when its method switches.
use erp_sdk::prelude::*;

pub(crate) fn validate(vals: &RawVals) -> Result<()> {
    let mut nonzero = false;
    for field in [
        "reservation_days_before",
        "reservation_days_before_priority",
    ] {
        match vals.get(field) {
            Some(Cell::Int(days)) if (0..=365).contains(days) => {
                nonzero |= *days != 0;
            }
            None => {} // absent new-record values take the declared zero default
            Some(_) => {
                return Err(Error::validation(format!(
                    "stock.picking.type.{field} must be between 0 and 365 days."
                )));
            }
        }
    }
    let by_date = matches!(vals.get("reservation_method"),Some(Cell::Str(method)) if method.as_ref()=="by_date");
    if nonzero && !by_date {
        return Err(Error::validation(
            "stock.picking.type reservation lead times must be zero unless reservation_method is by_date.",
        ));
    }
    Ok(())
}
pub(crate) async fn transition(
    operation: &Records,
    tx: &mut Transaction,
    old: &Cell,
) -> Result<()> {
    let method = operation.cell(tx, "reservation_method").await?;
    if method == *old {
        return Ok(());
    }
    let entering = method == Cell::Str("by_date".into());
    let leaving = *old == Cell::Str("by_date".into());
    if !entering && !leaving {
        return Ok(());
    }
    let states = Domain::field("state")?;
    let domain = Domain::and([
        Domain::field("picking_type_id")?.eq(operation.ids()[0]),
        if entering {
            states.in_(vec!["draft", "confirmed", "waiting", "partially_available"])
        } else {
            states.not_in(vec!["assigned", "done", "cancel"])
        },
    ]);
    let moves = operation
        .env()
        .records("stock.move")?
        .search_with(tx, &domain, &SearchOptions::new().order("id"))
        .await?;
    // Search applies record rules. A policy change must not silently reschedule only its
    // visible subset. This existence/count probe discloses no identity or count to the caller;
    // all actual reads and writes still use the ordinary authorized recordset.
    tx.flush().await?;
    let operation_id = operation.ids()[0];
    let rows=tx.query("SELECT count(*)::int8 FROM stock_move WHERE picking_type_id=$1 AND (( $2 AND state IN ('draft','confirmed','waiting','partially_available')) OR (NOT $2 AND state NOT IN ('assigned','done','cancel')))",&[&operation_id,&entering]).await?;
    let count: i64 = rows
        .first()
        .ok_or_else(|| Error::internal("Reservation dependency probe returned no row."))?
        .try_get(0)
        .map_err(|e| Error::db("Reservation dependency count is invalid.").with_source(e))?;
    let visible = i64::try_from(moves.len())
        .map_err(|e| Error::internal("Reservation move count is out of range.").with_source(e))?;
    if count != visible {
        return Err(Error::access(
            "You cannot change this reservation method because some affected stock moves are not accessible.",
        ));
    }
    if !entering {
        return moves
            .write(tx, RawVals::new().set_cell("reservation_date", Cell::Null))
            .await;
    }
    let Cell::Int(normal) = operation.cell(tx, "reservation_days_before").await? else {
        return Err(Error::internal(
            "Operation type has no normal reservation lead time.",
        ));
    };
    let Cell::Int(priority) = operation
        .cell(tx, "reservation_days_before_priority")
        .await?
    else {
        return Err(Error::internal(
            "Operation type has no priority reservation lead time.",
        ));
    };
    let dates = moves.cells(tx, "date").await?;
    let priorities = moves.cells(tx, "priority").await?;
    // Group equal dates so all moves with that date are written through one ORM call. The ORM
    // owns row/field access, invalidation and addon hooks; there is no SQL write bypass.
    let mut groups = std::collections::BTreeMap::<i32, Vec<i64>>::new();
    for ((id, date), p) in moves.ids().into_iter().zip(dates).zip(priorities) {
        let Cell::DateTime(date) = date else {
            return Err(Error::internal("Stock move has no scheduled date."));
        };
        let days = match p {
            Cell::Str(p) if p.as_ref() == "1" => priority,
            Cell::Str(p) if p.as_ref() == "0" => normal,
            _ => {
                return Err(Error::validation(
                    "Stock move priority must be 0 or 1 when scheduling reservation.",
                ));
            }
        };
        let date = date.date().days().checked_sub(days).ok_or_else(|| {
            Error::validation("Reservation date is outside the supported date range.")
        })?;
        groups.entry(date).or_default().push(id);
    }
    for (date, ids) in groups {
        moves
            .browse(ids)?
            .write(
                tx,
                RawVals::new().set_cell("reservation_date", Cell::Date(Date::from_days(date))),
            )
            .await?;
    }
    Ok(())
}
