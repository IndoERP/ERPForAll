// SPDX-License-Identifier: MIT OR Apache-2.0
//! Variant generator and attribute combination matrix engine.
//!
//! Handles:
//! - Materialization of PTAV records from PTAL value_ids.
//! - Cartesian product variant generation for attributes with mode 'always'.
//! - On-demand variant instantiation for attributes with mode 'dynamic'.
//! - Pruning of incompatible attribute combinations via PTAV exclusion rules.
//! - Dynamic variant generation limit enforcement.

use crate::template_attribute::ProductTemplateAttributeValue;
use erp_sdk::prelude::*;
use std::collections::{BTreeSet, HashMap, HashSet};

pub const VARIANT_LIMIT: usize = 1000;

/// Most combinations examined while looking for valid ones. Exclusion rules can discard almost
/// every combination of a huge product space; the search is cut loudly instead of running on.
pub const MAX_EXAMINED_COMBINATIONS: usize = 1_000_000;

/// Lazy Cartesian product of lists of ids: one combination at a time, the first list varying
/// fastest. It never builds the whole product, so the size of the space costs nothing until a
/// caller asks for the next item.
#[derive(Debug, Clone)]
pub struct Combinations<'a> {
    lists: &'a [Vec<i64>],
    positions: Vec<usize>,
    finished: bool,
}

impl<'a> Combinations<'a> {
    /// Combinations of one id out of each list. An empty `lists` or an empty member list gives
    /// no combination at all.
    pub fn new(lists: &'a [Vec<i64>]) -> Self {
        let finished = lists.is_empty() || lists.iter().any(Vec::is_empty);
        Self {
            lists,
            positions: vec![0; lists.len()],
            finished,
        }
    }
}

impl Iterator for Combinations<'_> {
    type Item = Vec<i64>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        let current = self
            .lists
            .iter()
            .zip(&self.positions)
            .map(|(list, &at)| list[at])
            .collect();
        // advance like an odometer whose first digit turns fastest
        let mut digit = 0;
        loop {
            if digit == self.positions.len() {
                self.finished = true;
                break;
            }
            self.positions[digit] += 1;
            if self.positions[digit] < self.lists[digit].len() {
                break;
            }
            self.positions[digit] = 0;
            digit += 1;
        }
        Some(current)
    }
}

/// Valid combinations (no exclusion hit), at most `limit + 1` of them. A result longer than
/// `limit` means "too many"; the product is never materialized beyond that point.
///
/// # Errors
///
/// `Validation` when more than [`MAX_EXAMINED_COMBINATIONS`] combinations were examined without
/// finding `limit + 1` valid ones.
pub fn valid_combinations(
    lists: &[Vec<i64>],
    exclusions: &HashMap<i64, Vec<i64>>,
    limit: usize,
) -> Result<Vec<Vec<i64>>> {
    let mut valid = Vec::new();
    for (examined, combo) in Combinations::new(lists).enumerate() {
        if examined >= MAX_EXAMINED_COMBINATIONS {
            return Err(Error::validation(format!(
                "The attribute lines of this product have too many combinations to examine                  (more than {MAX_EXAMINED_COMBINATIONS}). Remove values or use dynamic variants."
            )));
        }
        let excluded = combo.iter().any(|pid| {
            exclusions
                .get(pid)
                .is_some_and(|ex| ex.iter().any(|e| combo.contains(e)))
        });
        if excluded {
            continue;
        }
        valid.push(combo);
        if valid.len() > limit {
            break;
        }
    }
    Ok(valid)
}

/// Any other failure propagates.
async fn unlink_or_archive(tx: &mut Transaction, prod_model: &Records, ids: &[i64]) -> Result<()> {
    for &id in ids {
        let rec = prod_model.browse(vec![id])?;
        match tx.savepoint(async |tx| rec.unlink(tx).await).await {
            Ok(()) => {}
            Err(err) if is_still_referenced(&err) => {
                rec.write(tx, RawVals::new().set_cell("active", Cell::Bool(false)))
                    .await?;
            }
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

/// Whether `err` says the record cannot be deleted because something refers to it.
fn is_still_referenced(err: &Error) -> bool {
    matches!(err.kind(), erp_sdk::ErrorKind::UserError) || err.sqlstate() == Some("23503")
}

fn cell_to_id(c: Cell, field: &str) -> Result<i64> {
    match c {
        Cell::Id(id) => Ok(id),
        other => Err(Error::programming(format!(
            "field {field} returned {other:?}, expected an id"
        ))),
    }
}

fn cell_to_ids(c: Cell, field: &str) -> Result<Vec<i64>> {
    match c {
        Cell::Ids(ids) => Ok(ids.iter().map(|id| id.get()).collect()),
        Cell::Null => Ok(Vec::new()),
        other => Err(Error::programming(format!(
            "field {field} returned {other:?}, expected ids"
        ))),
    }
}

/// Synchronize PTAV records for a template attribute line.
pub async fn update_line_ptavs(tx: &mut Transaction, env: &Env, ptal_id: i64) -> Result<()> {
    let ptal_model = env.records("product.template.attribute.line")?;
    let ptav_model = env.records("product.template.attribute.value")?;
    let pav_model = env.records("product.attribute.value")?;

    let line = ptal_model.browse(vec![ptal_id])?;
    let tmpl_id = cell_to_id(line.cell(tx, "product_tmpl_id").await?, "product_tmpl_id")?;
    let attr_id = cell_to_id(line.cell(tx, "attribute_id").await?, "attribute_id")?;
    let line_val_ids = cell_to_ids(line.cell(tx, "value_ids").await?, "value_ids")?;

    // Query existing PTAVs for this line
    let domain = Domain::condition("attribute_line_id", Operator::Eq, ptal_id)?;
    let existing_ptavs = ptav_model.search(tx, &domain).await?;
    let mut existing_by_pav: HashMap<i64, i64> = HashMap::new();
    for ptav_rec_id in existing_ptavs.rec_ids() {
        let ptav = ptav_model.browse(vec![ptav_rec_id.get()])?;
        if let Cell::Id(pav_id) = ptav.cell(tx, "product_attribute_value_id").await? {
            existing_by_pav.insert(pav_id, ptav_rec_id.get());
        }
    }

    // Create missing PTAVs
    for pav_id in &line_val_ids {
        if !existing_by_pav.contains_key(pav_id) {
            let pav = pav_model.browse(vec![*pav_id])?;
            let extra = match pav.cell(tx, "default_extra_price").await? {
                Cell::Decimal(d) => d,
                _ => Decimal::ZERO,
            };
            let ptav_vals = RawVals::new()
                .set_cell("product_tmpl_id", Cell::Id(tmpl_id))
                .set_cell("attribute_line_id", Cell::Id(ptal_id))
                .set_cell("attribute_id", Cell::Id(attr_id))
                .set_cell("product_attribute_value_id", Cell::Id(*pav_id))
                .set_cell("price_extra", Cell::Decimal(extra));
            ptav_model.create(tx, vec![ptav_vals]).await?;
        }
    }

    // Remove PTAVs no longer in line_val_ids
    let stale: Vec<i64> = existing_by_pav
        .iter()
        .filter(|(pav_id, _)| !line_val_ids.contains(pav_id))
        .map(|(_, ptav_id)| *ptav_id)
        .collect();
    if !stale.is_empty() {
        ptav_model.browse(stale)?.unlink(tx).await?;
    }

    Ok(())
}

/// Generate or update product variants matrix for a product template.
///
/// A barcoded single-variant template keeps its barcode when the generator replaces that
/// variant: creating the first new variant ends the mirror, so when the
/// replaced variant is then deleted the barcode is written back to the template. An archived
/// replaced variant still holds it and the template stays cleared.
pub async fn sync_template_variants(tx: &mut Transaction, env: &Env, tmpl_id: i64) -> Result<()> {
    let tmpl = env.records("product.template")?.browse(vec![tmpl_id])?;
    let mirror = if crate::barcode::single_mirrors(tx, &[tmpl_id])
        .await?
        .is_empty()
    {
        None
    } else {
        match tmpl.cell(tx, "barcode").await? {
            Cell::Str(barcode) => Some(barcode),
            other => {
                return Err(Error::programming(format!(
                    "product.template.barcode returned {other:?} for a mirrored template, expected text"
                )));
            }
        }
    };
    sync_variants(tx, env, tmpl_id).await?;
    let Some(barcode) = mirror else {
        return Ok(());
    };
    let held = Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id)?
        & Domain::condition("barcode", Operator::Eq, &*barcode)?;
    let variants = env
        .records("product.product")?
        .with_context([("active_test", Value::Bool(false))]);
    if variants.search_count(tx, &held, None).await? == 0
        && tmpl.cell(tx, "barcode").await? != Cell::Str(barcode.clone())
    {
        tmpl.write(tx, RawVals::new().set_cell("barcode", Cell::Str(barcode)))
            .await?;
    }
    Ok(())
}

async fn sync_variants(tx: &mut Transaction, env: &Env, tmpl_id: i64) -> Result<()> {
    let tmpl_model = env.records("product.template")?;
    let ptal_model = env.records("product.template.attribute.line")?;
    let ptav_model = env.records("product.template.attribute.value")?;
    let attr_model = env.records("product.attribute")?;
    let prod_model = env.records("product.product")?;

    let tmpl = tmpl_model.browse(vec![tmpl_id])?;
    let line_ids = cell_to_ids(
        tmpl.cell(tx, "attribute_line_ids").await?,
        "attribute_line_ids",
    )?;

    if line_ids.is_empty() {
        // Single default variant without attributes
        let d = Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id)?;
        let existing = prod_model.search(tx, &d).await?;
        if existing.is_empty() {
            let vals = RawVals::new().set_cell("product_tmpl_id", Cell::Id(tmpl_id));
            prod_model.create(tx, vec![vals]).await?;
        }
        return Ok(());
    }

    // Inspect lines and attribute creation modes
    let mut ptav_lists: Vec<Vec<i64>> = Vec::new();
    let mut has_dynamic = false;

    for lid in line_ids {
        let line = ptal_model.browse(vec![lid])?;
        let is_active = match line.cell(tx, "active").await? {
            Cell::Bool(b) => b,
            other => {
                return Err(Error::programming(format!(
                    "product.template.attribute.line.active returned {other:?}, expected a boolean"
                )));
            }
        };
        if !is_active {
            continue;
        }

        let attr_id = cell_to_id(line.cell(tx, "attribute_id").await?, "attribute_id")?;
        let attr = attr_model.browse(vec![attr_id])?;
        let mode = match attr.cell(tx, "create_variant").await? {
            Cell::Str(s) => s.to_string(),
            other => {
                return Err(Error::programming(format!(
                    "product.attribute.create_variant returned {other:?}, expected text"
                )));
            }
        };

        if mode == "no_variant" {
            continue;
        }
        if mode == "dynamic" {
            has_dynamic = true;
            break;
        }

        // 'always': gather PTAV IDs
        let d_ptav = Domain::condition("attribute_line_id", Operator::Eq, lid)?;
        let line_ptavs = ptav_model.search(tx, &d_ptav).await?;
        let ids: Vec<i64> = line_ptavs.rec_ids().iter().map(|id| id.get()).collect();
        if !ids.is_empty() {
            ptav_lists.push(ids);
        }
    }

    if has_dynamic {
        // Superseded default blank variants must be unlinked.
        let d_prod = Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id)?;
        let existing = prod_model.search(tx, &d_prod).await?;
        let mut blank = Vec::new();
        for vid in existing.rec_ids() {
            let v = prod_model.browse(vec![vid.get()])?;
            let is_blank = match v.cell(tx, "combination_indices").await? {
                Cell::Str(ref s) => s.is_empty(),
                Cell::Null => true,
                other => {
                    return Err(Error::programming(format!(
                        "product.product.combination_indices returned {other:?}, expected text"
                    )));
                }
            };
            if is_blank {
                blank.push(vid.get());
            }
        }
        unlink_or_archive(tx, &prod_model, &blank).await?;
        return Ok(());
    }

    if ptav_lists.is_empty() {
        return Ok(());
    }

    // Build exclusion map across all relevant PTAVs
    let all_ptav_ids: Vec<i64> = ptav_lists.iter().flatten().copied().collect();
    let mut exclusion_map: HashMap<i64, Vec<i64>> = HashMap::new();
    if !all_ptav_ids.is_empty() {
        let ptav_recs = ptav_model.browse(all_ptav_ids.clone())?;
        let ptav_set = RecordSet::<ProductTemplateAttributeValue>::try_from(ptav_recs)?;
        let excluded_all = ptav_set
            .mapped(tx, ProductTemplateAttributeValue::EXCLUDED_VALUE_IDS)
            .await?;
        for (index, &pid) in all_ptav_ids.iter().enumerate() {
            let ex_ids: Vec<i64> = excluded_all[index].iter().map(|id| id.get()).collect();
            if !ex_ids.is_empty() {
                exclusion_map.insert(pid, ex_ids);
            }
        }
    }

    // Generate the valid combinations lazily and stop at the limit + 1: the
    // whole Cartesian product is never built.
    let found = valid_combinations(&ptav_lists, &exclusion_map, VARIANT_LIMIT)?;
    if found.len() > VARIANT_LIMIT {
        return Err(Error::validation(format!(
            "This change would create more than {VARIANT_LIMIT} variants, the most one product can have."
        )));
    }
    let mut valid_combos: Vec<String> = Vec::with_capacity(found.len());
    let mut combo_ptavs: Vec<Vec<i64>> = Vec::with_capacity(found.len());
    let mut valid_combos_set: HashSet<String> = HashSet::with_capacity(found.len());
    for mut sorted in found {
        sorted.sort_unstable();
        let serialized = sorted
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(",");
        valid_combos_set.insert(serialized.clone());
        valid_combos.push(serialized);
        combo_ptavs.push(sorted);
    }

    // Query existing variants of template
    let d_prod = Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id)?;
    let existing_variants = prod_model.search(tx, &d_prod).await?;
    let mut existing_combos: HashMap<String, i64> = HashMap::new();
    let mut obsolete_variants: Vec<i64> = Vec::new();

    for vid in existing_variants.rec_ids() {
        let v = prod_model.browse(vec![vid.get()])?;
        let indices = v.cell(tx, "combination_indices").await?;
        let comb_str = match indices {
            Cell::Str(ref s) => s.to_string(),
            Cell::Null => String::new(),
            other => {
                return Err(Error::programming(format!(
                    "product.product.combination_indices returned {other:?}, expected text"
                )));
            }
        };
        if !comb_str.is_empty() && valid_combos_set.contains(&comb_str) {
            existing_combos.insert(comb_str, vid.get());
        } else {
            // Obsolete, blank, or excluded combination variant
            obsolete_variants.push(vid.get());
        }
    }

    // Snapshot replacement metadata before deleting or archiving predecessors. Keep it local
    // to this template: an empty predecessor combination is a subset of every new combination.
    let all_variants = env
        .with_context([("active_test", Value::Bool(false))])
        .records("product.product")?
        .search(tx, &d_prod)
        .await?;
    let surviving: HashSet<i64> = existing_combos.values().copied().collect();
    let mut packaging_sources = Vec::new();
    for old in all_variants.iter() {
        if surviving.contains(&old.ids()[0]) {
            continue;
        }
        let extra = cell_to_ids(old.cell(tx, "extra_uom_ids").await?, "extra_uom_ids")?;
        if !extra.is_empty() {
            let combination: BTreeSet<i64> = cell_to_ids(
                old.cell(tx, "product_template_attribute_value_ids").await?,
                "product_template_attribute_value_ids",
            )?
            .into_iter()
            .collect();
            packaging_sources.push((combination, extra));
        }
    }

    // Create missing variants
    for (comb_str, ptavs) in valid_combos.iter().zip(combo_ptavs) {
        if !existing_combos.contains_key(comb_str) {
            let ptav_rec_ids: Vec<RecId> = ptavs
                .iter()
                .filter_map(|&id| RecId::from_real(id))
                .collect();
            let combination: BTreeSet<i64> = ptavs.iter().copied().collect();
            let mut extra = BTreeSet::new();
            for (old_combination, old_extra) in &packaging_sources {
                if old_combination.is_subset(&combination) {
                    extra.extend(old_extra.iter().copied());
                }
            }
            let prod_vals = RawVals::new()
                .set_cell(
                    "extra_uom_ids",
                    Cell::Ids(crate::packaging::rec_ids(extra)?.into()),
                )
                .set_cell("product_tmpl_id", Cell::Id(tmpl_id))
                .set_cell("combination_indices", Cell::Str(comb_str.as_str().into()))
                .set_cell(
                    "product_template_attribute_value_ids",
                    Cell::Ids(ptav_rec_ids.into()),
                );
            prod_model.create(tx, vec![prod_vals]).await?;
        }
    }

    unlink_or_archive(tx, &prod_model, &obsolete_variants).await?;

    Ok(())
}

/// On-demand variant creator for dynamic attributes.
pub async fn create_or_get_dynamic_variant(
    tx: &mut Transaction,
    env: &Env,
    tmpl_id: i64,
    ptav_ids: &[i64],
) -> Result<Records> {
    let prod_model = env.records("product.product")?;
    let ptav_model = env.records("product.template.attribute.value")?;

    let mut sorted = ptav_ids.to_vec();
    sorted.sort_unstable();
    let comb_str = sorted
        .iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(",");

    let d = Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id)?
        & Domain::condition("combination_indices", Operator::Eq, comb_str.as_str())?;
    let found = prod_model.search(tx, &d).await?;
    if !found.is_empty() {
        return Ok(found);
    }

    // Verify exclusions
    if !sorted.is_empty() {
        let ptav_recs = ptav_model.browse(sorted.clone())?;
        let ptav_set = RecordSet::<ProductTemplateAttributeValue>::try_from(ptav_recs)?;
        let excluded_all = ptav_set
            .mapped(tx, ProductTemplateAttributeValue::EXCLUDED_VALUE_IDS)
            .await?;
        for ex_ids in excluded_all {
            for ex in ex_ids {
                if sorted.contains(&ex.get()) {
                    return Err(Error::validation(
                        "Attribute combination is impossible by exclusion rules.",
                    ));
                }
            }
        }
    }

    let ptav_rec_ids: Vec<RecId> = sorted
        .iter()
        .filter_map(|&id| RecId::from_real(id))
        .collect();
    let prod_vals = RawVals::new()
        .set_cell("product_tmpl_id", Cell::Id(tmpl_id))
        .set_cell("combination_indices", Cell::Str(comb_str.into()))
        .set_cell(
            "product_template_attribute_value_ids",
            Cell::Ids(ptav_rec_ids.into()),
        );
    prod_model.create(tx, vec![prod_vals]).await
}
