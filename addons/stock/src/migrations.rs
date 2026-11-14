// SPDX-License-Identifier: MIT OR Apache-2.0
//! Preserve the former zero value of missing reservation lead times before schema checks.
use erp_sdk::migrations::SemverVersion;
use erp_sdk::prelude::*;

#[erp_sdk::migration(addon="stock",version="1.1",stage=pre)]
async fn reservation_lead_times(tx: &mut Transaction, _from: &SemverVersion) -> Result<()> {
    let rows = tx
        .query("SELECT to_regclass('stock_picking_type') IS NOT NULL", &[])
        .await?;
    let row = rows
        .first()
        .ok_or_else(|| Error::internal("Operation type migration probe returned no row."))?;
    let exists: bool = row
        .try_get(0)
        .map_err(|e| Error::db("Operation type migration probe failed.").with_source(e))?;
    if !exists {
        return Ok(());
    }
    // A legacy missing integer read as zero. Existing nonzero configuration is not reset or
    // clamped: the named checks refuse incompatible values and roll the upgrade back.
    tx.execute("UPDATE stock_picking_type SET reservation_days_before=0 WHERE reservation_days_before IS NULL",&[]).await?;
    tx.execute("UPDATE stock_picking_type SET reservation_days_before_priority=0 WHERE reservation_days_before_priority IS NULL",&[]).await?;
    Ok(())
}
