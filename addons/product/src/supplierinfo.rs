// SPDX-License-Identifier: MIT OR Apache-2.0
//! Product Supplier Info model (`product.supplierinfo`).

use crate::defaults::{company_currency_id, env_company_id, opt_id};
use crate::product::{ProductProduct, ProductProductMethods};
use erp_sdk::money::{ExactRatio, Increment, RoundingMode};
use erp_sdk::prelude::*;
use erp_sdk::{Operation, ValueMap};

/// `product.supplierinfo`: Vendor pricing agreement, delivery delays, and vendor SKU references.
#[model(
    name = "product.supplierinfo",
    description = "Supplier Price List",
    rec_name = "partner_id",
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
    #[field(
        float,
        digits = "Product Unit",
        default = 1,
        required = true,
        string = "Quantity"
    )]
    pub min_qty: Decimal,

    /// Agreed vendor unit price.
    #[field(float, digits = "Product Price", default = 0, string = "Unit Price")]
    pub price: Decimal,

    /// Vendor unit; a supplied unit survives product changes.
    #[field(
        many2one,
        comodel = "uom.uom",
        required = true,
        compute = "_compute_uom_id",
        store,
        readonly = false,
        string = "Unit"
    )]
    pub uom_id: Option<RecId>,

    /// Discounted vendor price expressed per main product unit.
    #[field(
        float,
        digits = "Product Price",
        compute = "_compute_price_discounted",
        string = "Discounted Price"
    )]
    pub price_discounted: Decimal,

    /// Current variant count of the linked template.
    #[field(integer, related = "product_tmpl_id.product_variant_count")]
    pub product_variant_count: i32,

    /// Agreed discount percentage.
    #[field(float, digits = "Discount", default = 0)]
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
    #[method(override)]
    async fn default_get(
        this: &RecordSet<ProductSupplierinfo>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<ValueMap> {
        let mut values: ValueMap = sup.call(tx, this.as_records(), (names.clone(),)).await?;
        if names.iter().any(|n| n == "company_id") && !values.contains_key("company_id") {
            values.insert(
                "company_id".into(),
                Value::Int(env_company_id(this.env(), "the default vendor company")?),
            );
        }
        if names.iter().any(|n| n == "currency_id") && !values.contains_key("currency_id") {
            let company = env_company_id(this.env(), "the default vendor currency")?;
            values.insert(
                "currency_id".into(),
                Value::Int(company_currency_id(tx, this.env(), company).await?),
            );
        }
        let mut links = RawVals::new();
        for name in ["product_id", "product_tmpl_id", "uom_id"] {
            let value = values
                .get(name)
                .cloned()
                .or_else(|| this.env().context().get(&format!("default_{name}")));
            if let Some(value) = value {
                let cell = match value {
                    Value::Int(id) => Cell::Id(id),
                    Value::False | Value::Bool(false) => Cell::Null,
                    other => {
                        return Err(Error::validation(format!(
                            "Vendor default {name} must be a record identity or false, found {other:?}."
                        )));
                    }
                };
                links.insert(name.into(), cell);
            }
        }
        let links = this._sanitize_vals(tx, links).await?;
        if names.iter().any(|n| n == "product_tmpl_id")
            && !values.contains_key("product_tmpl_id")
            && let Some(Cell::Id(id)) = links.get("product_tmpl_id")
        {
            values.insert("product_tmpl_id".into(), Value::Int(*id));
        }
        if names.iter().any(|n| n == "uom_id")
            && !values.contains_key("uom_id")
            && let Some(unit) = values_unit(this.env(), tx, &links).await?
        {
            values.insert("uom_id".into(), Value::Int(unit));
        }
        Ok(values)
    }

    /// Fill an omitted/empty template from an explicitly supplied variant; never repair a
    /// conflicting explicit template (BUG-088). The same hook serves create and write.
    #[method]
    async fn _sanitize_vals(
        this: &RecordSet<ProductSupplierinfo>,
        tx: &mut Transaction,
        mut values: RawVals,
    ) -> Result<RawVals> {
        if let Some(cell) = values.get("product_id")
            && let Some(variant) = opt_id(cell.clone(), "product_id")?
            && match values.get("product_tmpl_id") {
                None | Some(Cell::Null) => true,
                Some(cell) => opt_id(cell.clone(), "product_tmpl_id")?.is_none(),
            }
        {
            let product = this.env().records("product.product")?.browse([variant])?;
            values.insert(
                "product_tmpl_id".into(),
                product.cell(tx, "product_tmpl_id").await?,
            );
        }
        Ok(values)
    }

    #[method(override)]
    async fn create(
        this: &RecordSet<ProductSupplierinfo>,
        tx: &mut Transaction,
        vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        this.as_records()
            .check_access(tx, Operation::Create)
            .await?;
        tx.savepoint(async |tx| {
            let mut sanitized = Vec::with_capacity(vals_list.len());
            for values in vals_list {
                let mut values = this._sanitize_vals(tx, values).await?;
                if matches!(values.get("uom_id"), None | Some(Cell::Null)) {
                    if let Some(unit) = values_unit(this.env(), tx, &values).await? {
                        values.insert("uom_id".into(), Cell::Id(unit));
                    } else {
                        let defaults = this.as_records().default_get(tx, &["uom_id"]).await?;
                        if let Some(Value::Int(unit)) = defaults.get("uom_id") {
                            values.insert("uom_id".into(), Cell::Id(*unit));
                        }
                    }
                }
                sanitized.push(values);
            }
            sup.call(tx, this.as_records(), (sanitized,)).await
        })
        .await
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<ProductSupplierinfo>,
        tx: &mut Transaction,
        values: RawVals,
        sup: Super,
    ) -> Result<()> {
        this.as_records().check_access(tx, Operation::Write).await?;
        tx.savepoint(async |tx| {
            let values = this._sanitize_vals(tx, values).await?;
            sup.call(tx, this.as_records(), (values,)).await
        })
        .await
    }

    #[compute(depends = ["product_id", "product_tmpl_id"])]
    async fn _compute_uom_id(
        this: &RecordSet<ProductSupplierinfo>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ProductSupplierinfo>,
    ) -> Result<()> {
        for (index, id) in this.rec_ids().iter().enumerate() {
            let row =
                RecordSet::<ProductSupplierinfo>::try_from(this.as_records().browse_ids([*id]))?;
            let unit = match row.cell(tx, "uom_id").await? {
                Cell::Id(unit) => Some(unit),
                Cell::Null => product_unit(&row, tx).await?,
                other => {
                    return Err(Error::programming(format!(
                        "supplier uom_id returned {other:?}, expected an id or null"
                    )));
                }
            };
            let unit = unit
                .map(|id| {
                    RecId::from_raw(id)
                        .ok_or_else(|| Error::programming("Supplier unit identity cannot be zero."))
                })
                .transpose()?;
            out.set(index, ProductSupplierinfo::UOM_ID, unit)?;
        }
        Ok(())
    }

    /// Explicit helper: agreed prices are not bound to cost automatically.
    #[method]
    async fn _compute_price(
        this: &RecordSet<ProductSupplierinfo>,
        tx: &mut Transaction,
    ) -> Result<()> {
        for id in this.rec_ids() {
            let row =
                RecordSet::<ProductSupplierinfo>::try_from(this.as_records().browse_ids([*id]))?;
            let product = linked_product(&row, tx).await?;
            let price = match product {
                Some(product) if product.model_name() == "product.product" => {
                    let product = RecordSet::<ProductProduct>::try_from(product)?;
                    let values = product._get_standard_price(tx).await?;
                    let [price] = values.as_slice() else {
                        return Err(Error::programming(
                            "Supplier cost hook must return one value per product.",
                        ));
                    };
                    *price
                }
                Some(product) => {
                    decimal(product.cell(tx, "standard_price").await?, "standard_price")?
                }
                None => Decimal::ZERO,
            };
            row.as_records()
                .write(tx, RawVals::new().set_cell("price", Cell::Decimal(price)))
                .await?;
        }
        Ok(())
    }

    #[compute(depends = ["price", "discount", "uom_id", "product_id", "product_tmpl_id", "product_id.uom_id", "product_tmpl_id.uom_id", "uom_id.factor", "product_id.uom_id.factor", "product_tmpl_id.uom_id.factor"])]
    async fn _compute_price_discounted(
        this: &RecordSet<ProductSupplierinfo>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ProductSupplierinfo>,
    ) -> Result<()> {
        for (index, id) in this.rec_ids().iter().enumerate() {
            let row =
                RecordSet::<ProductSupplierinfo>::try_from(this.as_records().browse_ids([*id]))?;
            let value: ExactRatio = row
                .as_records()
                .call(tx, "_get_discounted_price", ())
                .await?;
            let decimal = rounded_price(row.env(), tx, &value)
                .await
                .map_err(|error| agreement_error(id.get(), error))?;
            out.set(index, ProductSupplierinfo::PRICE_DISCOUNTED, decimal)?;
        }
        Ok(())
    }

    /// Exact normalized price for native selection/valuation callers; never materialized
    /// through a rounded Decimal (A1).
    #[method]
    async fn _get_discounted_price(
        this: &RecordSet<ProductSupplierinfo>,
        tx: &mut Transaction,
    ) -> Result<ExactRatio> {
        this.ensure_one()?;
        discounted_exact(this.as_records(), tx)
            .await
            .map_err(|error| agreement_error(this.ids()[0], error))
    }

    /// Clear only the isolated form's incompatible variant. Stored writes retain BUG-088.
    #[onchange("product_tmpl_id")]
    async fn _onchange_product_tmpl_id(
        this: &RecordSet<ProductSupplierinfo>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        let row = this.as_records();
        let variant = opt_id(row.cell(tx, "product_id").await?, "product_id")?;
        if let Some(variant) = variant {
            let product = this.env().records("product.product")?.browse([variant])?;
            if product.cell(tx, "product_tmpl_id").await? != row.cell(tx, "product_tmpl_id").await?
            {
                row.write_base(tx, RawVals::new().set_cell("product_id", Cell::Null))
                    .await?;
            }
        }
        Ok(OnchangeResult::none())
    }

    /// Agreement result; record references are integer identities in native dynamic calls.
    #[method]
    async fn _get_seller_info(
        this: &RecordSet<ProductSupplierinfo>,
        tx: &mut Transaction,
    ) -> Result<ValueMap> {
        if this.is_empty() {
            return Ok(ValueMap::new());
        }
        this.ensure_one()?;
        let mut values = ValueMap::new();
        values.insert("supplierinfo".into(), Value::Int(this.ids()[0]));
        for name in [
            "partner_id",
            "price",
            "discount",
            "currency_id",
            "uom_id",
            "min_qty",
            "delay",
        ] {
            values.insert(name.into(), Value::from_cell(&this.cell(tx, name).await?));
        }
        let exact = this._get_discounted_price(tx).await?;
        values.insert(
            "price_discounted".into(),
            Value::Decimal(
                rounded_price(this.env(), tx, &exact)
                    .await
                    .map_err(|error| agreement_error(this.ids()[0], error))?,
            ),
        );
        Ok(values)
    }

    /// Native result retains both the ordinary mapping and the unrounded normalized price.
    #[method]
    async fn _get_seller_info_exact(
        this: &RecordSet<ProductSupplierinfo>,
        tx: &mut Transaction,
    ) -> Result<Option<crate::sellers::SellerInfo>> {
        if this.is_empty() {
            return Ok(None);
        }
        this.ensure_one()?;
        let exact = this._get_discounted_price(tx).await?;
        let values = this._get_seller_info(tx).await?;
        Ok(Some(crate::sellers::SellerInfo::new(values, exact)))
    }

    /// Caller-visible rows eligible for this exact company and variant.
    #[method]
    async fn _get_filtered_supplier(
        this: &RecordSet<ProductSupplierinfo>,
        tx: &mut Transaction,
        company: i64,
        product: i64,
        partner: Option<i64>,
    ) -> Result<Records> {
        let mut ids = Vec::new();
        for id in this.rec_ids() {
            let row =
                RecordSet::<ProductSupplierinfo>::try_from(this.as_records().browse_ids([*id]))?;
            let row_company = opt_id(row.cell(tx, "company_id").await?, "company_id")?;
            if row_company.is_some_and(|id| id != company) {
                continue;
            }
            if opt_id(row.cell(tx, "product_id").await?, "product_id")?
                .is_some_and(|id| id != product)
            {
                continue;
            }
            let vendor = required_id(row.cell(tx, "partner_id").await?, "partner_id")?;
            if partner.is_some_and(|id| id != vendor) {
                continue;
            }
            let vendor = this.env().records("res.partner")?.browse([vendor])?;
            match vendor.cell(tx, "active").await? {
                Cell::Bool(true) => ids.push(*id),
                Cell::Bool(false) => {}
                other => {
                    return Err(Error::programming(format!(
                        "Vendor active returned {other:?}, expected a boolean."
                    )));
                }
            }
        }
        Ok(this.as_records().browse_ids(ids))
    }

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

pub(crate) fn decimal(cell: Cell, name: &str) -> Result<Decimal> {
    match cell {
        Cell::Decimal(value) => Ok(value),
        // Optional numeric fields cleared by the caller or left null by older schemas
        // have the same zero meaning as the typed Decimal field reader.
        Cell::Null => Ok(Decimal::ZERO),
        other => Err(Error::programming(format!(
            "Supplier {name} returned {other:?}, expected a decimal."
        ))),
    }
}

fn agreement_error(id: i64, error: Error) -> Error {
    if error.kind() == erp_sdk::ErrorKind::Validation {
        Error::validation(format!("Vendor agreement {id}: {}", error.message())).with_source(error)
    } else {
        // Preserve access/database/missing kinds and their original diagnostics unchanged.
        error
    }
}

pub(crate) fn required_id(cell: Cell, name: &str) -> Result<i64> {
    opt_id(cell, name)?
        .ok_or_else(|| Error::validation(format!("Vendor agreement requires {name}.")))
}

async fn linked_product(
    row: &RecordSet<ProductSupplierinfo>,
    tx: &mut Transaction,
) -> Result<Option<Records>> {
    for (field, model) in [
        ("product_id", "product.product"),
        ("product_tmpl_id", "product.template"),
    ] {
        if let Some(id) = opt_id(row.cell(tx, field).await?, field)? {
            return Ok(Some(row.env().records(model)?.browse([id])?));
        }
    }
    Ok(None)
}

async fn values_unit(env: &Env, tx: &mut Transaction, values: &RawVals) -> Result<Option<i64>> {
    for (field, model) in [
        ("product_id", "product.product"),
        ("product_tmpl_id", "product.template"),
    ] {
        if let Some(cell) = values.get(field)
            && let Some(id) = opt_id(cell.clone(), field)?
        {
            return opt_id(
                env.records(model)?.browse([id])?.cell(tx, "uom_id").await?,
                "uom_id",
            );
        }
    }
    Ok(None)
}

async fn product_unit(
    row: &RecordSet<ProductSupplierinfo>,
    tx: &mut Transaction,
) -> Result<Option<i64>> {
    match linked_product(row, tx).await? {
        Some(product) => opt_id(product.cell(tx, "uom_id").await?, "uom_id"),
        None => Ok(None),
    }
}

pub(crate) async fn discounted_exact(row: &Records, tx: &mut Transaction) -> Result<ExactRatio> {
    let typed = RecordSet::<ProductSupplierinfo>::try_from(row.clone())?;
    let target = product_unit(&typed, tx)
        .await?
        .ok_or_else(|| Error::validation("Vendor discounted price requires a product unit."))?;
    let source = required_id(row.cell(tx, "uom_id").await?, "uom_id")?;
    let unit = erp_addon_uom::models::load_unit(row.env(), tx, source).await?;
    let target = erp_addon_uom::models::load_unit(row.env(), tx, target).await?;
    let price = decimal(row.cell(tx, "price").await?, "price")?;
    let discount = decimal(row.cell(tx, "discount").await?, "discount")?;
    let factor = ExactRatio::from_i64(1).checked_sub(
        &ExactRatio::from_decimal(discount)?.checked_div(&ExactRatio::from_i64(100))?,
    )?;
    Ok(erp_addon_uom::conversion::price_exact(&unit, &target, price)?.checked_mul(&factor)?)
}

pub(crate) async fn rounded_price(
    env: &Env,
    tx: &mut Transaction,
    value: &ExactRatio,
) -> Result<Decimal> {
    let digits = erp_addon_base::decimal_precision::precision_get(env, tx, "Product Price").await?;
    Ok(value.quantize(
        &Increment::from_places(digits)?,
        RoundingMode::HalfAwayFromZero,
    )?)
}
