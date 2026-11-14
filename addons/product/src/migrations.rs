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

/// Give legacy attribute records their former always-visible state before schema sync.
/// Existing archive choices, sequences, prices and relation identities are preserved.
///
/// # Errors
/// Database or decode errors propagate; the enclosing module-upgrade transaction rolls back.
#[erp_sdk::migration(addon = "product", version = "1.4", stage = pre)]
pub async fn migrate_attribute_activity(tx: &mut Transaction, _from: &SemverVersion) -> Result<()> {
    let rows = tx.query("SELECT to_regclass('product_attribute') IS NOT NULL, to_regclass('product_attribute_value') IS NOT NULL", &[]).await?;
    let row = rows
        .first()
        .ok_or_else(|| Error::programming("Attribute activity migration probe returned no row."))?;
    let attribute: bool = row
        .try_get(0)
        .map_err(|e| Error::db("Attribute migration table probe failed.").with_source(e))?;
    let value: bool = row
        .try_get(1)
        .map_err(|e| Error::db("Attribute value migration table probe failed.").with_source(e))?;
    // SQL defaults are forbidden for ORM-owned columns. Backfill existing rows explicitly;
    // new-record defaults remain on the field declarations (/134).
    if attribute {
        tx.batch(
            "ALTER TABLE product_attribute ADD COLUMN IF NOT EXISTS active boolean;
            UPDATE product_attribute SET active=true WHERE active IS NULL;",
        )
        .await?;
    }
    if value {
        tx.batch(
            "ALTER TABLE product_attribute_value ADD COLUMN IF NOT EXISTS active boolean;
            UPDATE product_attribute_value SET active=true WHERE active IS NULL;",
        )
        .await?;
    }
    Ok(())
}

/// Protect historical catalog and packaging unit identities before new hooks are activated.
/// Seek pagination bounds transient input's existing admission ceiling. The
/// lifecycle transaction owns the backfill, including repeat-upgrade and failure rollback.
///
/// # Errors
/// Invalid or unreadable historical units, corrupt guard state and database errors propagate.
#[erp_sdk::migration(addon = "product", version = "1.3", stage = post)]
pub async fn migrate_packaging_references(
    tx: &mut Transaction,
    _from: &SemverVersion,
) -> Result<()> {
    let env = tx.env();
    let mut after = 0_i64;
    let bound = i64::try_from(erp_addon_uom::references::MAX_GUARD_IDENTITIES).map_err(|_| {
        Error::programming("The UoM guard batch bound cannot fit a database limit.")
    })?;
    loop {
        let rows = tx
            .query(
                "SELECT uom_id FROM (
            SELECT uom_id FROM product_template
            UNION SELECT uom_id FROM product_uom
            UNION SELECT uom_id FROM product_template_uom_rel
            UNION SELECT uom_id FROM product_variant_extra_uom_rel
        ) units WHERE uom_id > $1 ORDER BY uom_id LIMIT $2",
                &[&after, &bound],
            )
            .await?;
        if rows.is_empty() {
            break;
        }
        let ids: Vec<i64> = rows
            .iter()
            .map(|row| {
                row.try_get(0)
                    .map_err(|e| Error::db(format!("Historical product unit identity: {e}")))
            })
            .collect::<Result<_>>()?;
        erp_addon_uom::references::register(&env, tx, &ids).await?;
        after = *ids
            .last()
            .ok_or_else(|| Error::programming("The historical unit batch has no last identity."))?;
    }
    Ok(())
}

/// Attach the new Products privilege to an existing unclassified manager group.
///
/// The group's noupdate data is not replayed on upgrade. An existing custom privilege is
/// preserved, and this versioned migration never reattaches a deliberately removed link
/// on subsequent ordinary upgrades. The ORM write maintains the group graph and policy signal.
///
/// # Errors
/// Missing or mistyped XML identities and missing target records refuse the upgrade;
/// database and ORM failures propagate to the lifecycle transaction for atomic rollback.
#[erp_sdk::migration(addon = "product", version = "1.2", stage = post)]
pub async fn migrate_product_access(tx: &mut Transaction, _from: &SemverVersion) -> Result<()> {
    let manager = security_identity(tx, "group_product_manager", "res.groups").await?;
    let privilege =
        security_identity(tx, "res_groups_privilege_product", "res.groups.privilege").await?;
    let group = tx.env().records("res.groups")?.browse([manager])?;
    // Read the target as well: an XML identity pointing at a deleted privilege must fail loudly.
    tx.env()
        .records("res.groups.privilege")?
        .browse([privilege])?
        .cell(tx, "name")
        .await?;
    if group.cell(tx, "privilege_id").await? == Cell::Null {
        group
            .write(
                tx,
                RawVals::new().set_cell("privilege_id", Cell::Id(privilege)),
            )
            .await?;
    }
    Ok(())
}

async fn security_identity(tx: &mut Transaction, name: &str, model: &str) -> Result<i64> {
    let rows = tx
        .query(
            "SELECT model, res_id FROM ir_model_data WHERE module='product' AND name=$1",
            &[&name],
        )
        .await?;
    let [row] = rows.as_slice() else {
        return Err(Error::programming(format!(
            "Product access migration requires exactly one product.{name} identity."
        )));
    };
    let actual: String = row
        .try_get(0)
        .map_err(|error| Error::db(format!("Product access identity model: {error}")))?;
    if actual != model {
        return Err(Error::programming(format!(
            "Product access migration requires product.{name} to name {model}, found {actual}."
        )));
    }
    row.try_get(1)
        .map_err(|error| Error::db(format!("Product access identity target: {error}")))
}

/// Build exact barcode claims after the new integrity tables exist.
///
/// # Errors
/// Refuses pre-existing duplicate holders with their model and row identities. The lifecycle
/// transaction rolls back schema, claim data and version advancement together on failure.
#[erp_sdk::migration(addon = "product", version = "1.5", stage = post)]
pub async fn migrate_barcode_claims(tx: &mut Transaction, _from: &SemverVersion) -> Result<()> {
    crate::barcode::backfill(tx).await
}
