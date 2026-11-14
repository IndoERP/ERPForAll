// SPDX-License-Identifier: MIT OR Apache-2.0
//! Transactional catalog unit use and extension hooks.
use crate::packaging::relation_ids;
use crate::{ProductProduct, ProductTemplate};
use erp_sdk::Operation;
use erp_sdk::prelude::*;
use std::collections::BTreeSet;

pub(crate) async fn register_units(
    records: &Records,
    tx: &mut Transaction,
    fields: &[&str],
) -> Result<()> {
    let mut ids = BTreeSet::new();
    for field in fields {
        for value in records.cells(tx, field).await? {
            match value {
                Cell::Id(id) => {
                    ids.insert(id);
                }
                Cell::Ids(_) => {
                    ids.extend(relation_ids(value, field)?);
                }
                // Optional/draft links have no persistent unit to register.
                Cell::Null => {}
                _ => {
                    return Err(Error::programming(format!(
                        "Product unit field {field} has an invalid value."
                    )));
                }
            }
        }
    }
    let ids: Vec<_> = ids.into_iter().collect();
    for chunk in ids.chunks(erp_addon_uom::references::MAX_GUARD_IDENTITIES) {
        erp_addon_uom::references::register(records.env(), tx, chunk).await?;
    }
    Ok(())
}

pub(crate) async fn before_template_unit_write(
    records: &Records,
    tx: &mut Transaction,
    vals: &RawVals,
) -> Result<()> {
    let Some(value) = vals.get("uom_id") else {
        return Ok(());
    };
    let Cell::Id(target) = value else {
        return Err(Error::validation(
            "Product main unit must be a positive unit identity.",
        ));
    };
    if *target <= 0 {
        return Err(Error::validation(
            "Product main unit must be a positive unit identity.",
        ));
    }
    records.check_access(tx, Operation::Write).await?;
    let mut changed = Vec::new();
    for template in records.iter() {
        if template.cell(tx, "uom_id").await? != Cell::Id(*target) {
            changed.extend(template.ids());
        }
    }
    if changed.is_empty() {
        return Ok(());
    }
    // Acquire the reference boundary before an addon can enter later inventory lock classes.
    erp_addon_uom::references::register(records.env(), tx, &[*target]).await?;
    let elevated = records.env().sudo(true).with_context([
        ("active_test", Value::Bool(false)),
        ("skip_uom_conversion", Value::Bool(true)),
    ]);
    let products = elevated
        .records("product.product")?
        .search(
            tx,
            &Domain::condition("product_tmpl_id", Operator::In, changed)?,
        )
        .await?;
    let success: bool = products.call(tx, "_update_uom", (*target,)).await?;
    if !success {
        return Err(Error::user(
            "The product unit-change extension refused the new unit.",
        ));
    }
    Ok(())
}

async fn unit_warning(
    records: &Records,
    tx: &mut Transaction,
    template: bool,
) -> Result<OnchangeResult> {
    records.ensure_one()?;
    let origin = records.origin();
    if origin.is_empty() {
        return Ok(OnchangeResult::none());
    }
    let old = origin.related(tx, "uom_id").await?;
    let new = records.related(tx, "uom_id").await?;
    if old.ids() == new.ids() || old.is_empty() || new.is_empty() {
        return Ok(OnchangeResult::none());
    }
    let products = if template {
        records
            .with_context([("active_test", Value::Bool(false))])
            .related(tx, "product_variant_ids")
            .await?
    } else {
        records.clone()
    };
    let warn: bool = products.call(tx, "_trigger_uom_warning", ()).await?;
    if !warn {
        return Ok(OnchangeResult::none());
    }
    let label = |cell| match cell {
        Cell::Str(s) => Ok(s),
        _ => Err(Error::programming(
            "The product unit warning needs a unit label.",
        )),
    };
    let old_name = label(old.cell(tx, "name").await?)?;
    let new_name = label(new.cell(tx, "name").await?)?;
    Ok(OnchangeResult::warning(
        "Changing the product unit",
        format!(
            "Existing documents will replace {old_name} with {new_name} one for one. Their quantities will stay the same. Review the affected documents before saving."
        ),
    ))
}
#[methods(ProductTemplate, addon = "product")]
impl ProductTemplate {
    #[onchange("uom_id")]
    async fn _onchange_uom_id(
        this: &RecordSet<ProductTemplate>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        unit_warning(this.as_records(), tx, true).await
    }
}
#[methods(ProductProduct, addon = "product")]
impl ProductProduct {
    #[onchange("uom_id")]
    async fn _onchange_uom_id(
        this: &RecordSet<ProductProduct>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        unit_warning(this.as_records(), tx, false).await
    }
}
