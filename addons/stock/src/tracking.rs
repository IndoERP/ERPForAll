// SPDX-License-Identifier: MIT OR Apache-2.0
//! Changing a product's tracking while lots, serial numbers or open lines exist
//! (6).
//!
//! The product's `tracking` field and its write path belong to; that path calls
//! [`check_tracking_change`] with the old and the new value of every product it writes, in the same
//! transaction as the write, and a refusal rolls the write back. This module owns the decision and the
//! user-facing sentence, so no other path words the refusal differently.
//!
//! What the reference does: it lets every change through and only warns when stock without a lot
//! remains, and an untracked product's quants are
//! summed whatever their lot. Here a change that
//! has to rewrite lot or serial rows (the serial positions, the open serial children, the lot of a
//! quant a serial rule now governs) is refused until the writers of those rows exist (
//! ); the changes that rewrite nothing go through.

use std::collections::BTreeMap;

use erp_sdk::prelude::*;

use crate::error::StockError;

/// A tracking mode of a product: the selection of the product's `tracking` field, where empty means
/// untracked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tracking {
    /// No lots or serial numbers (the field is empty).
    Untracked,
    /// By lot: many units under one lot number.
    Lot,
    /// By serial number: one unit per number.
    Serial,
}

impl Tracking {
    /// The mode named by a stored selection value; a value outside none, `lot` and `serial` is refused.
    pub fn from_selection(value: Option<&str>) -> Result<Self> {
        match value {
            None | Some("") => Ok(Self::Untracked),
            Some("lot") => Ok(Self::Lot),
            Some("serial") => Ok(Self::Serial),
            Some(other) => Err(Error::validation(format!(
                "Unknown tracking mode '{other}': use lot, serial or none."
            ))),
        }
    }
}

/// One product whose tracking is being changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackingChange {
    /// The product variant.
    pub product_id: i64,
    /// The mode it has now.
    pub from: Tracking,
    /// The mode it is being given.
    pub to: Tracking,
}

/// Distinct serial identities of a serial product that hold stock or open work: a position or lot
/// quant of non-zero quantity, or an open active child of an open active line. A lot is counted once
/// however many rows name it; a child that has no lot yet counts by its lot name, else as itself.
const SERIAL_ROWS: &str = "SELECT product_id, count(DISTINCT ident)::bigint FROM ( \
    SELECT product_id, 'l' || lot_id::text AS ident FROM stock_serial_position \
      WHERE product_id = ANY($1) AND quantity <> 0 \
    UNION ALL \
    SELECT product_id, 'l' || lot_id::text FROM stock_quant \
      WHERE product_id = ANY($1) AND lot_id IS NOT NULL AND (quantity <> 0 OR reserved_quantity <> 0) \
    UNION ALL \
    SELECT l.product_id, COALESCE('l' || s.lot_id::text, 'n' || s.lot_name, 'r' || s.id::text) \
      FROM stock_move_line_serial s JOIN stock_move_line l ON l.id = s.line_id \
      WHERE l.product_id = ANY($1) AND l.membership = 'open' AND l.active AND s.active \
  ) t GROUP BY product_id";

/// The same for a lot product: lot quants of non-zero quantity and open active lines naming a lot.
const LOT_ROWS: &str = "SELECT product_id, count(DISTINCT ident)::bigint FROM ( \
    SELECT product_id, 'l' || lot_id::text AS ident FROM stock_quant \
      WHERE product_id = ANY($1) AND lot_id IS NOT NULL AND (quantity <> 0 OR reserved_quantity <> 0) \
    UNION ALL \
    SELECT product_id, COALESCE('l' || lot_id::text, 'n' || lot_name) FROM stock_move_line \
      WHERE product_id = ANY($1) AND membership = 'open' AND active AND NOT child_mode \
        AND (lot_id IS NOT NULL OR lot_name IS NOT NULL) \
  ) t GROUP BY product_id";

/// Refuse the batch when a change needs lot or serial rows rewritten and a product has some.
///
/// The check is keyed on what the NEW mode cannot hold, so a path of several steps cannot defeat it
/// (cycle 12 S2: lot to untracked to serial left a lot quant on a serial product). Free steps: no
/// change, a change from untracked to lot (existing stock stays without a lot, as in the reference),
/// and lot to untracked (lots stay on their quants and lines). Checked: leaving serial (serial
/// rows), and entering serial from any other mode (lot rows). One query per kind covers the whole
/// batch; the refusal names the lowest blocked product id, the number of lots, serial numbers or open
/// serial rows found, and the task that will convert them (6).
pub async fn check_tracking_change(tx: &mut Transaction, changes: &[TrackingChange]) -> Result<()> {
    let leaving_serial =
        |c: &&TrackingChange| c.from == Tracking::Serial && c.to != Tracking::Serial;
    let entering_serial =
        |c: &&TrackingChange| c.from != Tracking::Serial && c.to == Tracking::Serial;
    for (sql, ids) in [
        (
            SERIAL_ROWS,
            changes
                .iter()
                .filter(leaving_serial)
                .map(|c| c.product_id)
                .collect::<Vec<i64>>(),
        ),
        (
            LOT_ROWS,
            changes
                .iter()
                .filter(entering_serial)
                .map(|c| c.product_id)
                .collect(),
        ),
    ] {
        if ids.is_empty() {
            continue;
        }
        // the read is SQL: pending ORM writes of lots or serial rows are flushed first; the product
        // rows are locked so a concurrent insert naming them (its foreign-key share lock) retries
        // the check instead of passing it unseen
        tx.flush().await?;
        crate::quant_guards::lock_guarded(
            tx,
            "SELECT FROM product_product WHERE id = ANY($1) ORDER BY id FOR UPDATE NOWAIT",
            &ids,
            "products",
        )
        .await?;
        let mut counts = BTreeMap::new();
        for row in tx.query(sql, &[&ids]).await? {
            let decode = |e| Error::internal(format!("tracking change read: {e}"));
            let product: i64 = row.try_get(0).map_err(decode)?;
            let count: i64 = row.try_get(1).map_err(decode)?;
            counts.insert(product, count);
        }
        if let Some((&product_id, &count)) = counts.iter().find(|(_, count)| **count > 0) {
            let name = tx
                .env()
                .records("product.product")?
                .browse(vec![product_id])?
                .cell(tx, "display_name")
                .await?;
            let Cell::Str(product) = name else {
                return Err(Error::internal(format!(
                    "product {product_id} has no display name"
                )));
            };
            return Err(StockError::TrackingChangeBlocked {
                product: product.to_string(),
                count,
            }
            .into());
        }
    }
    Ok(())
}
