// SPDX-License-Identifier: MIT OR Apache-2.0
//! Versioned product upgrades; the lifecycle transaction owns all schema/data changes.

use erp_sdk::migrations::SemverVersion;
use erp_sdk::prelude::*;

/// Backfill vendor units before the required column is synchronized (A2).
/// Existing non-null units and all negotiated values are preserved. The lifecycle caller
/// rolls back this migration and the whole upgrade when any source link is invalid.
///
/// # Errors
/// Validation names the total invalid row count and a bounded identity sample. Database
/// errors propagate. No product/unit or supplier record is manufactured as a fallback.
#[erp_sdk::migration(addon = "product", version = "1.1", stage = pre)]
pub async fn migrate_supplier_units(tx: &mut Transaction, _from: &SemverVersion) -> Result<()> {
    let exists: bool = tx
        .query(
            "SELECT to_regclass('product_supplierinfo') IS NOT NULL",
            &[],
        )
        .await?
        .first()
        .ok_or_else(|| Error::programming("Vendor migration table probe returned no row."))?
        .try_get(0)
        .map_err(|e| Error::db(format!("Vendor migration table probe: {e}")))?;
    if !exists {
        return Ok(());
    }
    tx.batch(
        "LOCK TABLE product_supplierinfo IN ACCESS EXCLUSIVE MODE;
        ALTER TABLE product_supplierinfo ADD COLUMN IF NOT EXISTS uom_id bigint;",
    )
    .await?;
    // Ten identities bound diagnostic output only; this is not a configurable record/admission limit.
    let invalid = tx.query("SELECT s.id, count(*) OVER () FROM product_supplierinfo s
        LEFT JOIN product_template t ON t.id = s.product_tmpl_id
        LEFT JOIN product_product p ON p.id = s.product_id
        LEFT JOIN uom_uom u ON u.id = t.uom_id
        LEFT JOIN uom_uom v ON v.id = s.uom_id
        WHERE t.id IS NULL OR u.id IS NULL
           OR (s.product_id IS NOT NULL AND (p.id IS NULL OR p.product_tmpl_id IS DISTINCT FROM s.product_tmpl_id))
           OR (s.uom_id IS NOT NULL AND v.id IS NULL)
        ORDER BY s.id LIMIT 10", &[]).await?;
    if let Some(first) = invalid.first() {
        let count: i64 = first
            .try_get(1)
            .map_err(|e| Error::db(format!("Vendor migration invalid count: {e}")))?;
        let mut ids = Vec::with_capacity(invalid.len());
        for row in invalid {
            let id: i64 = row
                .try_get(0)
                .map_err(|e| Error::db(format!("Vendor migration invalid identity: {e}")))?;
            ids.push(id.to_string());
        }
        return Err(Error::validation(format!(
            "Vendor unit migration refused {count} supplier rows with missing or inconsistent product/unit links; first IDs: {}.",
            ids.join(", ")
        )));
    }
    tx.execute(
        "UPDATE product_supplierinfo s SET uom_id = t.uom_id
        FROM product_template t WHERE s.product_tmpl_id = t.id AND s.uom_id IS NULL",
        &[],
    )
    .await?;
    // The old nullable Decimal field read NULL as zero. Preserve that effective threshold
    // before the new required-field synchronization can apply its new-record default of one.
    tx.execute(
        "UPDATE product_supplierinfo SET min_qty = 0 WHERE min_qty IS NULL",
        &[],
    )
    .await?;
    Ok(())
}
