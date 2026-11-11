//! Barcode uniqueness and packaging collision engine.
//!
//! Two transactions that assign the same barcode at the same time are serialized
//! with a transaction-scoped advisory lock per barcode (partial): the second one
//! waits for the first to finish. Under read committed it then sees the committed row; under
//! repeatable read its snapshot predates the lock and it can still store a duplicate. Each lock
//! holds a slot of PostgreSQL's shared lock table until commit, so very large barcoded batches are
//! bounded by `max_locks_per_transaction`. A claim row under a unique constraint is the open
//! replacement (review r2 SHOULD 1).

use erp_sdk::prelude::*;

/// Take a transaction-scoped advisory lock for each barcode, in sorted order so two batches never
/// lock in opposite orders. The lock is released when the transaction ends.
async fn lock_barcodes<'a>(
    tx: &mut Transaction,
    barcodes: impl Iterator<Item = &'a str>,
) -> Result<()> {
    let mut keys: Vec<&str> = barcodes.filter(|b| !b.is_empty()).collect();
    keys.sort_unstable();
    keys.dedup();
    for key in keys {
        tx.execute(
            "SELECT pg_advisory_xact_lock(hashtextextended($1::text, 7001))",
            &[&key],
        )
        .await?;
    }
    Ok(())
}

/// Validate barcode uniqueness across product variants, companies, templates, and packagings.
pub async fn check_barcode_uniqueness(
    tx: &mut Transaction,
    env: &Env,
    barcodes_by_record: &[(i64, Option<i64>, Option<i64>, String)], // (variant_id, tmpl_id, company_id, barcode)
) -> Result<()> {
    let ctx = env.context();
    if ctx
        .get("skip_barcode_uniqueness_check")
        .map(|v| v.is_truthy())
        .unwrap_or(false)
    {
        return Ok(());
    }

    // Check intra-batch duplicate collisions
    let mut seen_batch: std::collections::HashSet<(Option<i64>, &str)> =
        std::collections::HashSet::new();
    for &(_var_id, _tmpl_id, comp_id, ref barcode) in barcodes_by_record {
        let b = barcode.trim();
        if b.is_empty() {
            continue;
        }
        if seen_batch.contains(&(comp_id, b)) {
            return Err(Error::validation(format!(
                "- Barcode \"{b}\" already assigned in batch\n\nNote: Some products may be hidden due to access restrictions."
            )));
        }
        seen_batch.insert((comp_id, b));
    }

    let sudo = env.sudo(true);
    let prod_model = sudo.records("product.product")?;
    let tmpl_model = sudo.records("product.template")?;
    let uom_model = sudo.records("product.uom")?;
    tx.flush().await?;
    lock_barcodes(tx, barcodes_by_record.iter().map(|r| r.3.trim())).await?;

    for &(record_id, tmpl_id_opt, comp_opt, ref barcode) in barcodes_by_record {
        let b = barcode.trim();
        if b.is_empty() {
            continue;
        }

        // 1. Check against other product.product variants
        let d_bc = Domain::condition("barcode", Operator::Eq, b)?;
        let d_id = Domain::condition("id", Operator::Ne, record_id)?;

        let var_domain = if let Some(comp_id) = comp_opt {
            let d_comp = Domain::condition("company_id", Operator::Eq, comp_id)?;
            let d_shared = Domain::condition("company_id", Operator::Eq, false)?;
            d_bc.clone() & d_id & (d_comp | d_shared)
        } else {
            d_bc.clone() & d_id
        };

        let conflict_vars = prod_model.search(tx, &var_domain).await?;
        if !conflict_vars.is_empty() {
            // The collision is found as the superuser, but the name of the other product is only
            // given to a caller who may read it (no disclosure of hidden records).
            let readable_name = match conflict_vars.rec_ids().first() {
                Some(first_id) => {
                    let own = env
                        .records("product.product")?
                        .browse(vec![first_id.get()])?;
                    match own.cell(tx, "name").await {
                        Ok(Cell::Str(n)) => Some(n.to_string()),
                        Ok(_) => None,
                        Err(e)
                            if matches!(
                                e.kind(),
                                erp_sdk::ErrorKind::Access | erp_sdk::ErrorKind::Missing
                            ) =>
                        {
                            None
                        }
                        Err(e) => return Err(e),
                    }
                }
                None => None,
            };
            let Some(other_name) = readable_name else {
                return Err(Error::validation(format!(
                    "- Barcode \"{b}\" is already used by a product you cannot access\n\nNote: Some products may be hidden due to access restrictions."
                )));
            };
            return Err(Error::validation(format!(
                "- Barcode \"{b}\" already assigned to product: {other_name}\n\nNote: Some products may be hidden due to access restrictions."
            )));
        }

        // 2. Check against product.template
        let tmpl_domain = if let Some(comp_id) = comp_opt {
            let d_comp = Domain::condition("company_id", Operator::Eq, comp_id)?;
            let d_shared = Domain::condition("company_id", Operator::Eq, false)?;
            d_bc.clone() & (d_comp | d_shared)
        } else {
            d_bc.clone()
        };

        let conflict_tmpls = tmpl_model.search(tx, &tmpl_domain).await?;
        for tid in conflict_tmpls.rec_ids() {
            let conflict_tmpl_id = tid.get();
            // Single-variant mirroring exception:
            if Some(conflict_tmpl_id) == tmpl_id_opt {
                let d_all_vars =
                    Domain::condition("product_tmpl_id", Operator::Eq, conflict_tmpl_id)?;
                let var_count = prod_model.search_count(tx, &d_all_vars, None).await?;
                if var_count <= 1 {
                    continue; // Single variant mirroring allowed!
                }
            }
            // Multi-variant or different template barcode collision!
            return Err(Error::validation(format!(
                "- Barcode \"{b}\" already assigned to template\n\nNote: Some products may be hidden due to access restrictions."
            )));
        }

        // 3. Check against packaging product.uom
        let uom_domain = if let Some(comp_id) = comp_opt {
            let uom_comp = Domain::condition("company_id", Operator::Eq, comp_id)?;
            let uom_shared = Domain::condition("company_id", Operator::Eq, false)?;
            d_bc.clone() & (uom_comp | uom_shared)
        } else {
            d_bc.clone()
        };
        if uom_model.search_count(tx, &uom_domain, Some(1)).await? > 0 {
            return Err(Error::validation("A packaging already uses the barcode"));
        }
    }
    Ok(())
}

/// Validate barcode uniqueness across templates and variants.
pub async fn check_template_barcode_uniqueness(
    tx: &mut Transaction,
    env: &Env,
    barcodes_by_template: &[(i64, Option<i64>, String)], // (tmpl_id, company_id, barcode)
) -> Result<()> {
    let ctx = env.context();
    if ctx
        .get("skip_barcode_uniqueness_check")
        .map(|v| v.is_truthy())
        .unwrap_or(false)
    {
        return Ok(());
    }

    let sudo = env.sudo(true);
    let prod_model = sudo.records("product.product")?;
    let tmpl_model = sudo.records("product.template")?;
    tx.flush().await?;
    lock_barcodes(tx, barcodes_by_template.iter().map(|r| r.2.trim())).await?;

    for &(tmpl_id, comp_opt, ref barcode) in barcodes_by_template {
        let b = barcode.trim();
        if b.is_empty() {
            continue;
        }

        // Check against other templates
        let d_bc = Domain::condition("barcode", Operator::Eq, b)?;
        let d_id = Domain::condition("id", Operator::Ne, tmpl_id)?;
        let tmpl_domain = if let Some(comp_id) = comp_opt {
            let d_comp = Domain::condition("company_id", Operator::Eq, comp_id)?;
            let d_shared = Domain::condition("company_id", Operator::Eq, false)?;
            d_bc.clone() & d_id & (d_comp | d_shared)
        } else {
            d_bc.clone() & d_id
        };
        if tmpl_model.search_count(tx, &tmpl_domain, None).await? > 0 {
            return Err(Error::validation(format!(
                "- Barcode \"{b}\" already assigned to template\n\nNote: Some products may be hidden due to access restrictions."
            )));
        }

        // Check against variants
        let var_domain = if let Some(comp_id) = comp_opt {
            let d_comp = Domain::condition("company_id", Operator::Eq, comp_id)?;
            let d_shared = Domain::condition("company_id", Operator::Eq, false)?;
            d_bc.clone() & (d_comp | d_shared)
        } else {
            d_bc.clone()
        };

        let variants = prod_model.search(tx, &var_domain).await?;
        for v in variants.rec_ids() {
            let var_rec = prod_model.browse(vec![v.get()])?;
            let var_tmpl_id = match var_rec.cell(tx, "product_tmpl_id").await? {
                Cell::Id(id) => Some(id),
                _ => None,
            };
            if var_tmpl_id == Some(tmpl_id) {
                // If single-variant template: allow mirroring!
                let d_all = Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id)?;
                if prod_model.search_count(tx, &d_all, None).await? <= 1 {
                    continue;
                }
            }
            return Err(Error::validation(format!(
                "- Barcode \"{b}\" already assigned to product\n\nNote: Some products may be hidden due to access restrictions."
            )));
        }
    }
    Ok(())
}
