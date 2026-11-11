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
    description = "Link between products and their UoMs",
    order = "id",
    rec_name = "barcode",
    constraints = [(
        "_barcode_uniq",
        "UNIQUE(barcode)",
        "A barcode can only be assigned to one packaging."
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
        if let Some(company) = this.env().company_id()? {
            for vals in &mut vals_list {
                if !vals.contains("company_id") {
                    vals.insert("company_id".into(), Cell::Id(company));
                }
            }
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }

    /// A packaging barcode must not equal the barcode of any product. The search
    /// runs as the superuser so records hidden from the caller still collide.
    #[constrains("barcode")]
    async fn _check_barcode_uniqueness(
        this: &RecordSet<ProductUom>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let barcodes: Vec<String> = this
            .mapped(tx, ProductUom::BARCODE)
            .await?
            .into_iter()
            .flatten()
            .filter(|b| !b.is_empty())
            .map(|b| b.to_string())
            .collect();
        if barcodes.is_empty() {
            return Ok(());
        }
        let sudo = this.env().sudo(true);
        let products = sudo.records("product.product")?;
        let domain = Domain::condition("barcode", Operator::In, barcodes)?;
        if products.search_count(tx, &domain, Some(1)).await? > 0 {
            return Err(Error::validation("A product already uses the barcode"));
        }
        Ok(())
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
        string = "Barcodes"
    )]
    pub product_uom_ids: Vec<RecId>,
}
