// SPDX-License-Identifier: MIT OR Apache-2.0
//! Packaging units of a product (`product.uom`) and the packaging relations they add to
//! `uom.uom` and `product.product`.
//!
//! A row links a product to a unit with its own barcode (for example a carton of 12). The barcode
//! is unique among packagings (`_barcode_uniq`) and must not equal any product
//! barcode. The product-side half of that rule lives in [`crate::barcode`].

use erp_addon_uom::Uom;
use erp_sdk::prelude::*;

/// `product.uom`: a product packaged in a unit, identified by its own barcode.
#[model(
    name = "product.uom",
    description = "Product unit with its own barcode",
    order = "id",
    rec_name = "barcode",
    constraints = [(
        "_barcode_uniq",
        "UNIQUE(barcode)",
        "This barcode is already used by another packaging."
    )],
    company_access = direct(field = "company_id", branch = "ancestor", empty = "allow"),
)]
pub struct ProductUom {
    /// Packaging unit.
    #[field(
        many2one,
        comodel = "uom.uom",
        required = true,
        ondelete = "cascade",
        index = true,
        string = "Unit"
    )]
    pub uom_id: Option<RecId>,

    /// Packaged product.
    #[field(
        many2one,
        comodel = "product.product",
        required = true,
        ondelete = "cascade",
        index = true,
        string = "Product"
    )]
    pub product_id: Option<RecId>,

    /// Packaging barcode.
    #[field(char, required = true, copy = false, index = "btree_not_null")]
    pub barcode: Option<Arc<str>>,

    /// Available product units and applicable vendor units; contextual and never stored.
    #[field(many2many, comodel = "uom.uom", compute = "_compute_allowed_uom_ids")]
    pub allowed_uom_ids: Vec<RecId>,

    /// Owning company; defaults to the active company of the environment.
    #[field(many2one, comodel = "res.company", index = true)]
    pub company_id: Option<RecId>,
}

#[methods(ProductUom, addon = "product")]
impl ProductUom {
    /// An environment without a company leaves it empty, which makes the packaging global.
    #[method(override)]
    async fn create(
        this: &RecordSet<ProductUom>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        tx.savepoint(async |tx| {
            crate::barcode::refuse_bypass(this.env())?;
            if let Some(company) = this.env().company_id()? {
                for vals in &mut vals_list {
                    if !vals.contains("company_id") {
                        vals.insert("company_id".into(), Cell::Id(company));
                    }
                }
            }
            let keys = crate::barcode::prepare(tx, this.env(), &[], &[], &vals_list).await?;
            let records: Records = sup.call(tx, this.as_records(), (vals_list,)).await?;
            crate::packaging_lifecycle::register_units(&records, tx, &["uom_id"]).await?;
            let ids: Vec<i64> = records.rec_ids().iter().map(|id| id.get()).collect();
            crate::barcode::reconcile(tx, this.env(), &[], &ids, keys).await?;
            Ok(records)
        })
        .await
    }

    /// Release the packaging claim in the same transaction as its owner.
    #[method(override)]
    async fn unlink(this: &RecordSet<ProductUom>, tx: &mut Transaction, sup: Super) -> Result<()> {
        crate::barcode::refuse_bypass(this.env())?;
        let ids: Vec<i64> = this
            .as_records()
            .rec_ids()
            .iter()
            .map(|id| id.get())
            .collect();
        let families = Vec::new();
        let keys = crate::barcode::prepare(tx, this.env(), &families, &ids, &[]).await?;
        sup.call(tx, this.as_records(), ()).await?;
        crate::barcode::reconcile(tx, this.env(), &families, &ids, keys).await
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<ProductUom>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        tx.savepoint(async |tx| {
            crate::barcode::refuse_bypass(this.env())?;
            let ids: Vec<i64> = this
                .as_records()
                .rec_ids()
                .iter()
                .map(|id| id.get())
                .collect();
            let relevant = vals.contains("barcode") || vals.contains("company_id");
            let keys = if relevant {
                crate::barcode::prepare(tx, this.env(), &[], &ids, std::slice::from_ref(&vals))
                    .await?
            } else {
                Vec::new()
            };
            let units_changed = vals.contains("uom_id");
            sup.call(tx, this.as_records(), (vals,)).await?;
            if units_changed {
                crate::packaging_lifecycle::register_units(this.as_records(), tx, &["uom_id"])
                    .await?;
            }
            if relevant {
                crate::barcode::reconcile(tx, this.env(), &[], &ids, keys).await?;
            }
            Ok(())
        })
        .await
    }
}

/// `uom.uom` gains the packaging barcodes of products that use the unit.
#[extend(Uom)]
pub struct UomUomPackagings {
    /// Packaging rows (barcodes) of this unit.
    #[field(
        one2many,
        comodel = "product.uom",
        inverse_name = "uom_id",
        string = "Barcodes",
        compute = "_compute_product_uom_ids",
        inverse = "_set_product_uom_ids",
        readonly = false
    )]
    pub product_uom_ids: Vec<RecId>,
}

/// Backing inverse relation for the context-dependent barcode projection.
#[extend(Uom)]
pub struct UomPackagingSource {
    /// Unfiltered inverse relation backing the contextual barcode projection.
    #[field(
        one2many,
        comodel = "product.uom",
        inverse_name = "uom_id",
        readonly = true
    )]
    pub all_product_uom_ids: Vec<RecId>,
}
