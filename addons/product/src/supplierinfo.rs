//! Product Supplier Info model (`product.supplierinfo`).

use erp_sdk::prelude::*;

/// `product.supplierinfo`: Vendor pricing agreement, delivery delays, and vendor SKU references.
#[model(
    name = "product.supplierinfo",
    description = "Supplier Price List",
    order = "sequence, min_qty desc, price, id",
    company_access = direct(field = "company_id", branch = "ancestor", empty = "allow"),
)]
pub struct ProductSupplierinfo {
    /// Vendor partner.
    #[field(
        many2one,
        comodel = "res.partner",
        required = true,
        ondelete = "cascade",
        index = true,
        string = "Vendor"
    )]
    pub partner_id: Option<RecId>,

    /// Target template.
    #[field(
        many2one,
        comodel = "product.template",
        required = true,
        ondelete = "cascade",
        index = true
    )]
    pub product_tmpl_id: Option<RecId>,

    /// Optional specific variant.
    #[field(many2one, comodel = "product.product", index = true)]
    pub product_id: Option<RecId>,

    /// Vendor-specific product description.
    #[field(char, string = "Vendor Product Name")]
    pub product_name: Option<Arc<str>>,

    /// Vendor-specific SKU reference.
    #[field(char, string = "Vendor Product Code")]
    pub product_code: Option<Arc<str>>,

    /// Sequence order.
    #[field(integer, default = 1)]
    pub sequence: i32,

    /// Minimum purchase quantity threshold.
    #[field(float, digits = 0, string = "Quantity")]
    pub min_qty: Decimal,

    /// Agreed vendor unit price.
    #[field(float, digits = "Product Price", string = "Unit Price")]
    pub price: Decimal,

    /// Agreed discount percentage.
    #[field(float, digits = "Discount")]
    pub discount: Decimal,

    /// Price currency.
    #[field(many2one, comodel = "res.currency", required = true)]
    pub currency_id: Option<RecId>,

    /// Validity start date.
    #[field(date)]
    pub date_start: Option<Date>,

    /// Validity end date.
    #[field(date)]
    pub date_end: Option<Date>,

    /// Delivery lead time in calendar days.
    #[field(integer, default = 1, string = "Lead Time")]
    pub delay: i32,

    /// Restricting company.
    #[field(many2one, comodel = "res.company", index = true)]
    pub company_id: Option<RecId>,
}

#[methods(ProductSupplierinfo, addon = "product")]
impl ProductSupplierinfo {
    /// Validate agreement date range.
    #[constrains("date_start", "date_end")]
    async fn _check_dates(
        this: &RecordSet<ProductSupplierinfo>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let starts = this.mapped(tx, ProductSupplierinfo::DATE_START).await?;
        let ends = this.mapped(tx, ProductSupplierinfo::DATE_END).await?;

        for (start_opt, end_opt) in starts.into_iter().zip(ends) {
            if let (Some(start), Some(end)) = (start_opt, end_opt)
                && start > end
            {
                return Err(Error::validation(
                    "The start date of the vendor price agreement cannot exceed the end date.",
                ));
            }
        }
        Ok(())
    }

    /// BUG-088 Fix: Model invariant ensuring variant belongs to declared template.
    #[constrains("product_id", "product_tmpl_id")]
    async fn _check_product_variant(
        this: &RecordSet<ProductSupplierinfo>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let prod_model = this.env().records("product.product")?;
        let var_ids = this.mapped(tx, ProductSupplierinfo::PRODUCT_ID).await?;
        let tmpl_ids = this
            .mapped(tx, ProductSupplierinfo::PRODUCT_TMPL_ID)
            .await?;

        for (var_opt, tmpl_opt) in var_ids.into_iter().zip(tmpl_ids) {
            if let (Some(var_id), Some(tmpl_id)) = (var_opt, tmpl_opt) {
                let var_rec = prod_model.browse(vec![var_id.get()])?;
                let parent_tmpl = var_rec.cell(tx, "product_tmpl_id").await?;
                if let Cell::Id(actual_tmpl_id) = parent_tmpl
                    && actual_tmpl_id != tmpl_id.get()
                {
                    return Err(Error::validation(format!(
                        "Supplierinfo variant {} does not belong to product template {}.",
                        var_id.get(),
                        tmpl_id.get()
                    )));
                }
            }
        }
        Ok(())
    }
}
