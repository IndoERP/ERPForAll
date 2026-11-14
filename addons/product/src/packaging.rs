// SPDX-License-Identifier: MIT OR Apache-2.0
//! Product packaging availability and contextual barcode relations.
use crate::{ProductProduct, ProductTemplate, ProductUom, UomUomPackagings};
use erp_addon_uom::Uom;
use erp_sdk::prelude::*;
use std::collections::BTreeSet;

/// Extract a relation without silently treating a wrong field type as empty.
pub(crate) fn relation_ids(cell: Cell, field: &str) -> Result<Vec<i64>> {
    match cell {
        Cell::Ids(ids) => Ok(ids.iter().map(|id| id.get()).collect()),
        // An unset relation on a new row is the empty relation.
        Cell::Null => Ok(Vec::new()),
        _ => Err(Error::programming(format!(
            "{field} must contain record identities."
        ))),
    }
}
pub(crate) fn rec_ids(ids: impl IntoIterator<Item = i64>) -> Result<Vec<RecId>> {
    ids.into_iter()
        .map(|id| {
            RecId::from_real(id)
                .ok_or_else(|| Error::validation("Packaging unit identities must be positive."))
        })
        .collect()
}
async fn feature_enabled(env: &Env, tx: &mut Transaction) -> Result<bool> {
    env.records("res.groups")?
        .call(tx, "_is_uom_feature_enabled", ())
        .await
}

/// Parse only the product context of a packaging screen. Context is a filter, never authority.
pub(crate) fn product_context(env: &Env) -> Result<(Option<i64>, Vec<i64>)> {
    let single = match env.context().get("product_id") {
        None | Some(Value::False) => None,
        Some(Value::Int(id)) if id > 0 => Some(id),
        _ => {
            return Err(Error::validation(
                "Packaging context product_id must be a positive integer.",
            ));
        }
    };
    let mut ids = BTreeSet::new();
    match env.context().get("product_ids") {
        None | Some(Value::False) => {}
        Some(Value::List(values)) => {
            // Reuse the unit guard's admission ceiling; this is not a business capacity setting.
            if values.len() > erp_addon_uom::references::MAX_GUARD_IDENTITIES {
                return Err(Error::validation(
                    "Packaging context product_ids exceeds the unit guard identity bound.",
                ));
            }
            for value in values.iter() {
                match value {
                    Value::Int(id) if *id > 0 => {
                        ids.insert(*id);
                    }
                    _ => {
                        return Err(Error::validation(
                            "Packaging context product_ids must contain positive integers.",
                        ));
                    }
                }
            }
        }
        _ => {
            return Err(Error::validation(
                "Packaging context product_ids must be an integer list.",
            ));
        }
    }
    Ok((single, ids.into_iter().collect()))
}

#[methods(ProductTemplate, addon = "product")]
impl ProductTemplate {
    /// Available units of one template; empty input produces no units.
    #[method]
    async fn _get_available_uoms(
        this: &RecordSet<ProductTemplate>,
        tx: &mut Transaction,
    ) -> Result<Records> {
        if this.is_empty() {
            return this.env().records("uom.uom");
        }
        this.ensure_one()?;
        let mut units = this.as_records().related(tx, "uom_id").await?;
        if feature_enabled(this.env(), tx).await? {
            units = units.union(&this.as_records().related(tx, "uom_ids").await?)?;
        }
        Ok(units)
    }
    /// Default unit of the selected template(s).
    #[method]
    async fn _get_main_uom(
        this: &RecordSet<ProductTemplate>,
        tx: &mut Transaction,
    ) -> Result<Records> {
        this.as_records().related(tx, "uom_id").await
    }
    /// Combos do not offer multiple-unit selection.
    #[method]
    async fn _has_multiple_uoms(
        this: &RecordSet<ProductTemplate>,
        tx: &mut Transaction,
    ) -> Result<bool> {
        this.ensure_one()?;
        if this.cell(tx, "type").await? == Cell::Str("combo".into()) {
            return Ok(false);
        }
        let units: Records = this
            .as_records()
            .call(tx, "_get_available_uoms", ())
            .await?;
        Ok(units.len() > 1)
    }
}

#[methods(ProductProduct, addon = "product")]
impl ProductProduct {
    /// Template units plus this variant's extra packaging when the global feature is enabled.
    #[method]
    async fn _get_available_uoms(
        this: &RecordSet<ProductProduct>,
        tx: &mut Transaction,
    ) -> Result<Records> {
        this.ensure_one()?;
        let template = this.as_records().related(tx, "product_tmpl_id").await?;
        let mut units: Records = template.call(tx, "_get_available_uoms", ()).await?;
        if feature_enabled(this.env(), tx).await? {
            units = units.union(&this.as_records().related(tx, "extra_uom_ids").await?)?;
        }
        Ok(units)
    }
    /// Main unit remains the template's unit.
    #[method]
    async fn _get_main_uom(
        this: &RecordSet<ProductProduct>,
        tx: &mut Transaction,
    ) -> Result<Records> {
        this.as_records()
            .related(tx, "product_tmpl_id")
            .await?
            .call(tx, "_get_main_uom", ())
            .await
    }
    /// Combos do not offer multiple-unit selection.
    #[method]
    async fn _has_multiple_uoms(
        this: &RecordSet<ProductProduct>,
        tx: &mut Transaction,
    ) -> Result<bool> {
        this.ensure_one()?;
        if this.cell(tx, "type").await? == Cell::Str("combo".into()) {
            return Ok(false);
        }
        let units: Records = this
            .as_records()
            .call(tx, "_get_available_uoms", ())
            .await?;
        Ok(units.len() > 1)
    }
    /// Extension hook: dependent addons replace or refuse business-document units here.
    /// Base product changes no dependent documents and returns success.
    #[method]
    async fn _update_uom(
        _this: &RecordSet<ProductProduct>,
        _tx: &mut Transaction,
        _to_uom_id: i64,
    ) -> Result<bool> {
        Ok(true)
    }
    /// Extension hook: base product has no business history requiring a warning.
    #[method]
    async fn _trigger_uom_warning(
        _this: &RecordSet<ProductProduct>,
        _tx: &mut Transaction,
    ) -> Result<bool> {
        Ok(false)
    }
}

#[methods(ProductUom, addon = "product")]
impl ProductUom {
    #[compute(depends = ["uom_id", "product_id.uom_id", "product_id.uom_ids", "product_id.extra_uom_ids", "product_id.seller_ids.uom_id", "product_id.seller_ids.product_id"], depends_context = ["active_model", "uid", "allowed_company_ids"])]
    async fn _compute_allowed_uom_ids(
        this: &RecordSet<ProductUom>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ProductUom>,
    ) -> Result<()> {
        for (index, row) in this.iter().enumerate() {
            let mut units = this.env().records("uom.uom")?;
            if this.env().context().get("active_model") == Some(Value::str("uom.uom")) {
                units = row.as_records().related(tx, "uom_id").await?;
            } else {
                let product = row.as_records().related(tx, "product_id").await?;
                if !product.is_empty() {
                    units = product.call(tx, "_get_available_uoms", ()).await?;
                    // Relations preserve draft origins; constructing an SQL domain from a
                    // draft template/product id would incorrectly require a database id.
                    for seller in product.related(tx, "seller_ids").await?.iter() {
                        let specific = seller.related(tx, "product_id").await?;
                        let same_origin = !specific.origin().is_empty()
                            && specific.origin().ids() == product.origin().ids();
                        if specific.is_empty()
                            || specific.rec_ids() == product.rec_ids()
                            || same_origin
                        {
                            units = units.union(&seller.related(tx, "uom_id").await?)?;
                        }
                    }
                }
            }
            out.set(index, ProductUom::ALLOWED_UOM_IDS, units.rec_ids().to_vec())?;
        }
        Ok(())
    }
}

#[methods(Uom, addon = "product")]
impl UomUomPackagings {
    #[method]
    async fn _set_product_uom_ids(this: &RecordSet<Uom>, tx: &mut Transaction) -> Result<()> {
        let (single, multiple) = product_context(this.env())?;
        let mut products: BTreeSet<i64> = multiple.into_iter().collect();
        products.extend(single);
        for unit in this.iter() {
            // This supported conversion turns draft CREATE/UPDATE rows into persistent
            // commands; treating their temporary RecIds as database ids is invalid.
            let commands = unit
                .as_records()
                .x2many_commands(tx, "product_uom_ids")
                .await?;
            let mut commands = commands.into_iter();
            let Some(Command::Set(desired)) = commands.next() else {
                return Err(Error::programming(
                    "The packaging inverse needs a normalized SET command.",
                ));
            };
            let desired: BTreeSet<i64> = desired.into_iter().map(|id| id.get()).collect();
            let mut domain = Domain::condition("uom_id", Operator::Eq, unit.ids()[0])?;
            if !products.is_empty() {
                domain = domain
                    & Domain::condition(
                        "product_id",
                        Operator::In,
                        products.iter().copied().collect::<Vec<_>>(),
                    )?;
            }
            let visible = this
                .env()
                .records("product.uom")?
                .search(tx, &domain)
                .await?;
            let existing: BTreeSet<i64> = visible.ids().into_iter().collect();
            // Change only caller-visible rows; neither another company's rows nor rows
            // outside product context are removed by an edit to this projection.
            let mut delta = Vec::new();
            for id in existing.difference(&desired) {
                delta.push(Command::unlink(rec_ids([*id])?[0]));
            }
            for id in desired.difference(&existing) {
                delta.push(Command::link(rec_ids([*id])?[0]));
            }
            if !desired.is_empty() || commands.len() > 0 {
                erp_addon_uom::references::register(this.env(), tx, &[unit.ids()[0]]).await?;
            }
            delta.extend(commands);
            unit.as_records()
                .write_base(
                    tx,
                    RawVals::new().set_commands("all_product_uom_ids", delta),
                )
                .await?;
        }
        Ok(())
    }

    /// Native writes to the backing relation retain the same reference boundary.
    #[constrains("all_product_uom_ids")]
    async fn _register_packaging_relation_units(
        this: &RecordSet<Uom>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let mut used = Vec::new();
        for unit in this.iter() {
            if !unit
                .as_records()
                .related(tx, "all_product_uom_ids")
                .await?
                .is_empty()
            {
                used.extend(unit.ids());
            }
        }
        for ids in used.chunks(erp_addon_uom::references::MAX_GUARD_IDENTITIES) {
            erp_addon_uom::references::register(this.env(), tx, ids).await?;
        }
        Ok(())
    }

    #[compute(depends = ["all_product_uom_ids.product_id", "all_product_uom_ids.uom_id"], depends_context = ["product_id", "product_ids", "uid", "allowed_company_ids"])]
    async fn _compute_product_uom_ids(
        this: &RecordSet<Uom>,
        tx: &mut Transaction,
        out: &mut Assign<'_, Uom>,
    ) -> Result<()> {
        let (single, multiple) = product_context(this.env())?;
        let mut products: BTreeSet<i64> = multiple.into_iter().collect();
        products.extend(single);
        for (index, unit) in this.iter().enumerate() {
            if unit.rec_ids()[0].is_new() {
                let mut selected = Vec::new();
                for row in unit
                    .as_records()
                    .related(tx, "all_product_uom_ids")
                    .await?
                    .iter()
                {
                    let product = row.related(tx, "product_id").await?;
                    if products.is_empty() || product.ids().iter().any(|id| products.contains(id)) {
                        selected.extend_from_slice(row.rec_ids());
                    }
                }
                out.set(index, UomUomPackagings::PRODUCT_UOM_IDS, selected)?;
                continue;
            }
            let mut domain =
                Domain::condition("uom_id", Operator::Eq, unit.ensure_one()?.ids()[0])?;
            if !products.is_empty() {
                domain = domain
                    & Domain::condition(
                        "product_id",
                        Operator::In,
                        products.iter().copied().collect::<Vec<_>>(),
                    )?;
            }
            let rows = this
                .env()
                .records("product.uom")?
                .search(tx, &domain)
                .await?;
            out.set(
                index,
                UomUomPackagings::PRODUCT_UOM_IDS,
                rows.rec_ids().to_vec(),
            )?;
        }
        Ok(())
    }
}
