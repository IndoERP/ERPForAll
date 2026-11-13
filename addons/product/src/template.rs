//! Product Template model (`product.template`).

use crate::defaults::{company_currency_id, default_unit_id, env_company_id, opt_id};
use crate::product::ProductProductMethods;
use erp_sdk::prelude::*;
use erp_sdk::{Operation, ValueMap};

/// Barcode, internal reference and cost a new single-variant template hands to its variant.
type Mirrored = (Option<Arc<str>>, Option<Arc<str>>, Decimal);

/// `product.template`: Abstract catalog entity defining common product properties.
#[model(
    name = "product.template",
    description = "Product",
    order = "name, id",
    rec_name = "name",
    rec_names_search = ["name", "default_code", "barcode"],
    company_access = direct(field = "company_id", branch = "ancestor", empty = "allow"),
)]
pub struct ProductTemplate {
    /// Catalog product name.
    #[field(char, required = true, translate = true, index = "trigram")]
    pub name: Option<Arc<str>>,

    /// Contextual catalog label including the internal reference when requested.
    #[field(char, compute = "_compute_display_name")]
    pub display_name: Option<Arc<str>>,

    /// Display sequence order.
    #[field(integer, default = 1)]
    pub sequence: i32,

    /// Internal / technical description.
    #[field(html, translate = true)]
    pub description: Option<Arc<str>>,

    /// Description for purchase orders and RFQs.
    #[field(text, translate = true, string = "Purchase Description")]
    pub description_purchase: Option<Arc<str>>,

    /// Description for sales orders and customer invoices.
    #[field(text, translate = true, string = "Sales Description")]
    pub description_sale: Option<Arc<str>>,

    /// Product type: 'consu' (Goods), 'service' (Service), 'combo' (Combo).
    #[field(
        selection,
        selection = [("consu", "Goods"), ("service", "Service"), ("combo", "Combo")],
        default = "consu",
        required = true,
        string = "Product Type"
    )]
    pub r#type: Option<Arc<str>>,

    /// Inventory tracking flag; reset to false when the type is not `consu`.
    #[field(
        boolean,
        default = false,
        compute = "_compute_is_storable",
        store = true,
        readonly = false,
        string = "Track Inventory"
    )]
    pub is_storable: bool,

    /// Classification category.
    #[field(
        many2one,
        comodel = "product.category",
        index = true,
        string = "Product Category"
    )]
    pub categ_id: Option<RecId>,

    /// Default unit of measure for operations.
    #[field(many2one, comodel = "uom.uom", required = true, string = "Unit")]
    pub uom_id: Option<RecId>,

    /// Catalog base sales price.
    #[field(float, digits = "Product Price", string = "Sales Price")]
    pub list_price: Decimal,

    /// Company-dependent unit cost, computed from variants with inverse.
    #[field(
        float,
        digits = "Product Price",
        compute = "_compute_standard_price",
        inverse = "_set_standard_price",
        readonly = false,
        string = "Cost"
    )]
    pub standard_price: Decimal,

    /// Currency of template (from company currency or default).
    #[field(many2one, comodel = "res.currency", compute = "_compute_currency_id")]
    pub currency_id: Option<RecId>,

    /// Cost currency of template.
    #[field(
        many2one,
        comodel = "res.currency",
        compute = "_compute_cost_currency_id"
    )]
    pub cost_currency_id: Option<RecId>,

    /// Restricting company (None means shared catalog across all companies).
    #[field(many2one, comodel = "res.company", index = true)]
    pub company_id: Option<RecId>,

    /// Active lifecycle flag. Archiving cascades to all linked variants.
    #[field(boolean, default = true)]
    pub active: bool,

    /// Mirrored or primary barcode.
    #[field(
        char,
        compute = "_compute_barcode",
        inverse = "_set_barcode",
        store = true,
        index = true,
        string = "Barcode"
    )]
    pub barcode: Option<Arc<str>>,

    /// Mirrored or primary SKU internal reference.
    #[field(
        char,
        compute = "_compute_default_code",
        inverse = "_set_default_code",
        store = true,
        index = true,
        string = "Internal Reference"
    )]
    pub default_code: Option<Arc<str>>,

    /// All linked product variants.
    #[field(
        one2many,
        comodel = "product.product",
        inverse_name = "product_tmpl_id",
        string = "Products"
    )]
    pub product_variant_ids: Vec<RecId>,

    /// Prefetched pointer to the first variant.
    #[field(
        many2one,
        comodel = "product.product",
        compute = "_compute_product_variant_id",
        string = "Product"
    )]
    pub product_variant_id: Option<RecId>,

    /// Count of variants belonging to this template.
    #[field(
        integer,
        compute = "_compute_product_variant_count",
        string = "# Product Variants"
    )]
    pub product_variant_count: i32,

    /// Attribute lines configuring variants.
    #[field(
        one2many,
        comodel = "product.template.attribute.line",
        inverse_name = "product_tmpl_id",
        string = "Product Attributes"
    )]
    pub attribute_line_ids: Vec<RecId>,

    /// Vendor pricing and lead times.
    #[field(
        one2many,
        comodel = "product.supplierinfo",
        inverse_name = "product_tmpl_id",
        string = "Vendors"
    )]
    pub seller_ids: Vec<RecId>,
}

#[methods(ProductTemplate, addon = "product")]
impl ProductTemplate {
    /// Template labels preserve the caller's code-display preferences.
    #[compute(depends = ["name", "default_code"], depends_context = ["display_default_code", "formatted_display_name", "lang"])]
    async fn _compute_display_name(
        this: &RecordSet<ProductTemplate>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ProductTemplate>,
    ) -> Result<()> {
        let names = this.mapped(tx, ProductTemplate::NAME).await?;
        let codes = this.mapped(tx, ProductTemplate::DEFAULT_CODE).await?;
        for (index, (name, code)) in names.into_iter().zip(codes).enumerate() {
            let label = name
                .filter(|v| !v.is_empty())
                .map(|name| crate::names::format_label(this.env(), &name, code.as_deref()))
                .transpose()?;
            out.set(
                index,
                ProductTemplate::DISPLAY_NAME,
                label.map(Arc::<str>::from),
            )?;
        }
        Ok(())
    }

    /// The web client's action
    /// normalizer (`erp_normalize_action_result`) admits `default_<field>` keys and, for a
    /// current-window action, boolean `create` and `active_test`; any other key is refused.
    #[method]
    #[rpc(effect = read)]
    async fn action_open_variants(
        this: &RecordSet<ProductTemplate>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let records = this.as_records();
        records.check_access(tx, Operation::Read).await?;
        let id = records.ensure_one()?.int()?;
        if records.exists(tx).await?.is_empty() {
            return Err(Error::missing(
                "The variants action needs an existing product.",
            ));
        }
        let context = ValueMap::from([
            ("create".to_owned(), Value::Bool(false)),
            ("default_product_tmpl_id".to_owned(), Value::Int(id)),
        ]);
        Ok(Value::Map(Arc::new(ValueMap::from([
            ("type".to_owned(), Value::str("ir.actions.act_window")),
            ("name".to_owned(), Value::str("Product Variants")),
            ("res_model".to_owned(), Value::str("product.product")),
            ("target".to_owned(), Value::str("current")),
            (
                "views".to_owned(),
                Value::list([
                    Value::list([Value::False, Value::str("list")]),
                    Value::list([Value::False, Value::str("form")]),
                ]),
            ),
            (
                "domain".to_owned(),
                Value::list([Value::list([
                    Value::str("product_tmpl_id"),
                    Value::str("="),
                    Value::Int(id),
                ])]),
            ),
            ("context".to_owned(), Value::Map(Arc::new(context))),
        ]))))
    }

    #[method(override)]
    async fn default_get(
        this: &RecordSet<ProductTemplate>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<ValueMap> {
        let mut res = sup.call(tx, this.as_records(), (names.clone(),)).await?;
        if names.iter().any(|n| n == "uom_id") && !res.contains_key("uom_id") {
            res.insert("uom_id".into(), Value::Int(default_unit_id(tx).await?));
        }
        if names.iter().any(|n| n == "list_price") && !res.contains_key("list_price") {
            res.insert("list_price".into(), Value::Decimal(Decimal::ONE));
        }
        Ok(res)
    }

    /// Automatically create default variant upon template creation,
    /// unless bypassed via context flag `create_product_product: false`.
    #[method(override)]
    async fn create(
        this: &RecordSet<ProductTemplate>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        if vals_list.is_empty() {
            return sup.call(tx, this.as_records(), (vals_list,)).await;
        }

        let ctx = this.env().context();
        let default_uom_id = if vals_list.iter().any(|v| !v.contains("uom_id")) {
            Some(default_unit_id(tx).await?)
        } else {
            None
        };

        // The variant mirrors barcode, reference and cost of a template created without
        // attribute lines; take them from the submitted values, before the variant exists.
        let mut mirrored: Vec<Mirrored> = Vec::new();
        for vals in &mut vals_list {
            if !vals.contains("uom_id")
                && let Some(uom_id) = default_uom_id
            {
                vals.insert("uom_id".into(), Cell::Id(uom_id));
            }
            if !vals.contains("list_price") {
                vals.insert("list_price".into(), Cell::Decimal(Decimal::ONE));
            }
            let text = |name: &str| match vals.get(name) {
                Some(Cell::Str(s)) if !s.is_empty() => Some(s.clone()),
                _ => None,
            };
            let price = match vals.get("standard_price") {
                Some(Cell::Decimal(d)) => *d,
                _ => Decimal::ZERO,
            };
            mirrored.push((text("barcode"), text("default_code"), price));
        }

        let records = sup.call(tx, this.as_records(), (vals_list,)).await?;

        let bypass_create_variant = ctx
            .get("create_product_product")
            .map(|v| !v.is_truthy())
            .unwrap_or(false);

        if !bypass_create_variant {
            let prod_model = this.env().records("product.product")?;
            for (tmpl_id, (barcode, default_code, standard_price)) in
                records.rec_ids().iter().zip(mirrored)
            {
                let tmpl = this.browse(vec![tmpl_id.get()])?;
                let attr_lines = tmpl.mapped(tx, ProductTemplate::ATTRIBUTE_LINE_IDS).await?;
                if attr_lines.iter().flatten().next().is_none() {
                    let mut prod_vals =
                        RawVals::new().set_cell("product_tmpl_id", Cell::Id(tmpl_id.get()));
                    if let Some(bc) = barcode {
                        prod_vals = prod_vals.set_cell("barcode", Cell::Str(bc));
                    }
                    if let Some(dc) = default_code {
                        prod_vals = prod_vals.set_cell("default_code", Cell::Str(dc));
                    }
                    if standard_price > Decimal::ZERO {
                        prod_vals =
                            prod_vals.set_cell("standard_price", Cell::Decimal(standard_price));
                    }
                    prod_model.create(tx, vec![prod_vals]).await?;
                }
            }
        }

        Ok(records)
    }

    /// Archive cascade: archiving template archives variants.
    #[method(override)]
    async fn write(
        this: &RecordSet<ProductTemplate>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        let is_active_change = vals.get("active").cloned();
        let barcode_change = vals.get("barcode").cloned();

        if let Some(Cell::Str(ref bc)) = barcode_change {
            let mut tmpl_tuples = Vec::new();
            for rec_id in this.as_records().rec_ids() {
                let rec = this.browse(vec![rec_id.get()])?;
                let comp_id = match vals.get("company_id") {
                    Some(c) => opt_id(c.clone(), "company_id")?,
                    None => opt_id(rec.cell(tx, "company_id").await?, "company_id")?,
                };
                tmpl_tuples.push((rec_id.get(), comp_id, bc.to_string()));
            }
            if !tmpl_tuples.is_empty() {
                crate::barcode::check_template_barcode_uniqueness(tx, this.env(), &tmpl_tuples)
                    .await?;
            }
        }

        if let Some(Cell::Bool(false)) = is_active_change {
            let combo_item_model = this.env().sudo(true).records("product.combo.item")?;
            let variant_ids = this
                .mapped(tx, ProductTemplate::PRODUCT_VARIANT_IDS)
                .await?;
            let ids: Vec<i64> = variant_ids
                .into_iter()
                .flatten()
                .map(|id| id.get())
                .collect();
            if !ids.is_empty() {
                let d = Domain::condition("product_id", Operator::In, ids)?;
                if combo_item_model.search_count(tx, &d, None).await? > 0 {
                    return Err(Error::validation(
                        "You cannot archive a product that is part of a combo. Please remove it from the following combos first.",
                    ));
                }
            }
        }

        sup.call(tx, this.as_records(), (vals,)).await?;

        if let Some(Cell::Str(ref bc)) = barcode_change {
            let prod_model = this.env().records("product.product")?;
            for rec_id in this.as_records().rec_ids() {
                let d_all = Domain::condition("product_tmpl_id", Operator::Eq, rec_id.get())?;
                let variants = prod_model.search(tx, &d_all).await?;
                if variants.len() == 1 {
                    let mut v_vals = RawVals::new();
                    v_vals.insert("barcode".into(), Cell::Str(bc.clone()));
                    variants.write(tx, v_vals).await?;
                }
            }
        }

        if let Some(Cell::Bool(active)) = is_active_change {
            let variant_ids = this
                .mapped(tx, ProductTemplate::PRODUCT_VARIANT_IDS)
                .await?;
            let ids: Vec<i64> = variant_ids
                .into_iter()
                .flatten()
                .map(|id| id.get())
                .collect();
            if !ids.is_empty() {
                let prod_model = this.env().records("product.product")?;
                let mut v_vals = RawVals::new();
                v_vals.insert("active".into(), Cell::Bool(active));
                let variants = prod_model.browse(ids)?;
                variants.write(tx, v_vals).await?;
            }
        }
        Ok(())
    }

    /// Compute pointer to first variant and total variant count.
    #[compute(depends = ["product_variant_ids"])]
    async fn _compute_product_variant_id(
        this: &RecordSet<ProductTemplate>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ProductTemplate>,
    ) -> Result<()> {
        let ids = this.as_records().rec_ids();
        let prod_model = this.env().records("product.product")?;

        for (index, tmpl_id) in ids.iter().enumerate() {
            let domain = Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id.get())?;
            let variants = prod_model.search(tx, &domain).await?;
            let first = variants.rec_ids().first().copied();
            out.set(index, ProductTemplate::PRODUCT_VARIANT_ID, first)?;
        }
        Ok(())
    }

    #[compute(depends = ["product_variant_ids"])]
    async fn _compute_product_variant_count(
        this: &RecordSet<ProductTemplate>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ProductTemplate>,
    ) -> Result<()> {
        let ids = this.as_records().rec_ids();
        let prod_model = this.env().records("product.product")?;

        for (index, tmpl_id) in ids.iter().enumerate() {
            let domain = Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id.get())?;
            let count = prod_model.search_count(tx, &domain, None).await?;
            let count = i32::try_from(count).map_err(|_| {
                Error::programming(format!("variant count {count} does not fit an i32"))
            })?;
            out.set(index, ProductTemplate::PRODUCT_VARIANT_COUNT, count)?;
        }
        Ok(())
    }

    /// A product that is not `consu` cannot be tracked in inventory.
    #[compute(depends = ["type"])]
    async fn _compute_is_storable(
        this: &RecordSet<ProductTemplate>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ProductTemplate>,
    ) -> Result<()> {
        let types = this.mapped(tx, ProductTemplate::TYPE).await?;
        let flags = this.mapped(tx, ProductTemplate::IS_STORABLE).await?;
        for (index, (kind, flag)) in types.into_iter().zip(flags).enumerate() {
            let storable = flag && kind.as_deref() == Some("consu");
            out.set(index, ProductTemplate::IS_STORABLE, storable)?;
        }
        Ok(())
    }

    /// Synchronize standard_price from variant to template.
    #[compute(depends = ["product_variant_ids"])]
    async fn _compute_standard_price(
        this: &RecordSet<ProductTemplate>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ProductTemplate>,
    ) -> Result<()> {
        let prod_model = this.env().records("product.product")?;
        for (index, tmpl_id) in this.as_records().rec_ids().iter().enumerate() {
            let domain = Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id.get())?;
            let variants = prod_model.search(tx, &domain).await?;
            if let Some(first_id) = variants.rec_ids().first() {
                // through Hook N6: a valuation addon may supply the cost of the variant
                let first = RecordSet::<crate::ProductProduct>::try_from(
                    prod_model.browse(vec![first_id.get()])?,
                )?;
                let price = first
                    ._get_standard_price(tx)
                    .await?
                    .into_iter()
                    .next()
                    .ok_or_else(|| {
                        Error::programming("_get_standard_price returned no value for a variant")
                    })?;
                out.set(index, ProductTemplate::STANDARD_PRICE, price)?;
            } else {
                out.set(index, ProductTemplate::STANDARD_PRICE, Decimal::ZERO)?;
            }
        }
        Ok(())
    }

    /// Inverse standard_price sets variant standard_price.
    #[method]
    async fn _set_standard_price(
        this: &RecordSet<ProductTemplate>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let prod_model = this.env().records("product.product")?;
        let prices = this.mapped(tx, ProductTemplate::STANDARD_PRICE).await?;

        for (tmpl_id, price) in this.as_records().rec_ids().iter().zip(prices) {
            let domain = Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id.get())?;
            let variants = prod_model.search(tx, &domain).await?;
            if !variants.is_empty() {
                let mut vals = RawVals::new();
                vals.insert("standard_price".into(), Cell::Decimal(price));
                variants.write(tx, vals).await?;
            }
        }
        Ok(())
    }

    /// Compute currency_id from company currency.
    #[compute(depends = ["company_id"])]
    async fn _compute_currency_id(
        this: &RecordSet<ProductTemplate>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ProductTemplate>,
    ) -> Result<()> {
        let comp_ids = this.mapped(tx, ProductTemplate::COMPANY_ID).await?;
        for (index, comp_opt) in comp_ids.into_iter().enumerate() {
            // a shared catalog entry takes the currency of the active company; no company at all
            // is refused instead of guessing one
            let target_comp_id = match comp_opt {
                Some(comp_id) => comp_id.get(),
                None => env_company_id(this.env(), "the currency of a product without company")?,
            };
            let currency_id = company_currency_id(tx, this.env(), target_comp_id).await?;
            out.set(
                index,
                ProductTemplate::CURRENCY_ID,
                RecId::from_real(currency_id),
            )?;
        }
        Ok(())
    }

    /// Compute cost_currency_id.
    #[compute(depends = ["company_id"])]
    async fn _compute_cost_currency_id(
        this: &RecordSet<ProductTemplate>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ProductTemplate>,
    ) -> Result<()> {
        Self::_compute_currency_id(this, tx, out).await
    }

    /// Synchronize barcode for single-variant products.
    #[compute(depends = ["product_variant_ids", "product_variant_ids.barcode"])]
    async fn _compute_barcode(
        this: &RecordSet<ProductTemplate>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ProductTemplate>,
    ) -> Result<()> {
        let prod_model = this.env().records("product.product")?;
        for (index, tmpl_id) in this.as_records().rec_ids().iter().enumerate() {
            let domain = Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id.get())?;
            let variants = prod_model.search(tx, &domain).await?;
            if variants.len() == 1 {
                let bc = match variants.cell(tx, "barcode").await? {
                    Cell::Str(s) => Some(s),
                    _ => None,
                };
                out.set(index, ProductTemplate::BARCODE, bc)?;
            }
        }
        Ok(())
    }

    /// Inverse barcode sets variant barcode on single-variant products.
    #[method]
    async fn _set_barcode(this: &RecordSet<ProductTemplate>, tx: &mut Transaction) -> Result<()> {
        let prod_model = this.env().records("product.product")?;
        let barcodes = this.mapped(tx, ProductTemplate::BARCODE).await?;

        for (tmpl_id, barcode_opt) in this.as_records().rec_ids().iter().zip(barcodes) {
            let domain = Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id.get())?;
            let variants = prod_model.search(tx, &domain).await?;
            if variants.len() == 1 {
                let mut vals = RawVals::new();
                if let Some(bc) = barcode_opt {
                    vals.insert("barcode".into(), Cell::Str(bc));
                } else {
                    vals.insert("barcode".into(), Cell::Null);
                }
                variants.write(tx, vals).await?;
            }
        }
        Ok(())
    }

    /// Synchronize default_code for single-variant products.
    #[compute(depends = ["product_variant_ids", "product_variant_ids.default_code"])]
    async fn _compute_default_code(
        this: &RecordSet<ProductTemplate>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ProductTemplate>,
    ) -> Result<()> {
        let prod_model = this.env().records("product.product")?;
        for (index, tmpl_id) in this.as_records().rec_ids().iter().enumerate() {
            let domain = Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id.get())?;
            let variants = prod_model.search(tx, &domain).await?;
            if variants.len() == 1 {
                let dc = match variants.cell(tx, "default_code").await? {
                    Cell::Str(s) => Some(s),
                    _ => None,
                };
                out.set(index, ProductTemplate::DEFAULT_CODE, dc)?;
            }
        }
        Ok(())
    }

    /// Inverse default_code sets variant default_code on single-variant products.
    #[method]
    async fn _set_default_code(
        this: &RecordSet<ProductTemplate>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let prod_model = this.env().records("product.product")?;
        let codes = this.mapped(tx, ProductTemplate::DEFAULT_CODE).await?;

        for (tmpl_id, code_opt) in this.as_records().rec_ids().iter().zip(codes) {
            let domain = Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id.get())?;
            let variants = prod_model.search(tx, &domain).await?;
            if variants.len() == 1 {
                let mut vals = RawVals::new();
                if let Some(dc) = code_opt {
                    vals.insert("default_code".into(), Cell::Str(dc));
                } else {
                    vals.insert("default_code".into(), Cell::Null);
                }
                variants.write(tx, vals).await?;
            }
        }
        Ok(())
    }
}

impl ProductTemplate {
    /// Generate or synchronize variant matrix for template.
    pub async fn create_variant_matrix(
        this: &RecordSet<ProductTemplate>,
        tx: &mut Transaction,
    ) -> Result<()> {
        for tmpl_id in this.as_records().rec_ids() {
            crate::variant_generator::sync_template_variants(tx, this.env(), tmpl_id.get()).await?;
        }
        Ok(())
    }

    /// On-demand variant creator for dynamic combinations.
    pub async fn create_or_get_dynamic_variant(
        this: &RecordSet<ProductTemplate>,
        tx: &mut Transaction,
        ptav_ids: &[i64],
    ) -> Result<Records> {
        let tmpl_id = this
            .as_records()
            .rec_ids()
            .first()
            .ok_or_else(|| Error::validation("No template"))?
            .get();
        crate::variant_generator::create_or_get_dynamic_variant(tx, this.env(), tmpl_id, ptav_ids)
            .await
    }
}
