//! Template Attribute Lines (PTAL) and Values (PTAV).

use erp_sdk::prelude::*;

/// `product.template.attribute.line`: Configuration line on template binding an attribute to values.
#[model(
    name = "product.template.attribute.line",
    description = "Product Template Attribute Line",
    order = "id",
    company_access = parent(edge = "product_tmpl_id"),
)]
pub struct ProductTemplateAttributeLine {
    /// Parent product template.
    #[field(
        many2one,
        comodel = "product.template",
        required = true,
        ondelete = "cascade",
        index = true
    )]
    pub product_tmpl_id: Option<RecId>,

    /// Associated attribute.
    #[field(
        many2one,
        comodel = "product.attribute",
        required = true,
        index = true,
        string = "Attribute"
    )]
    pub attribute_id: Option<RecId>,

    /// Allowed attribute values on this line.
    #[field(
        many2many,
        comodel = "product.attribute.value",
        relation = "product_template_attribute_line_value_rel",
        string = "Values"
    )]
    pub value_ids: Vec<RecId>,

    /// Stored materialized PTAV records.
    #[field(
        one2many,
        comodel = "product.template.attribute.value",
        inverse_name = "attribute_line_id"
    )]
    pub product_template_value_ids: Vec<RecId>,

    /// Active flag.
    #[field(boolean, default = true)]
    pub active: bool,
}

#[methods(ProductTemplateAttributeLine, addon = "product")]
impl ProductTemplateAttributeLine {
    /// Ensure active line has at least one value.
    #[constrains("active", "value_ids")]
    async fn _check_valid_values(
        this: &RecordSet<ProductTemplateAttributeLine>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let actives = this
            .mapped(tx, ProductTemplateAttributeLine::ACTIVE)
            .await?;
        let all_values = this
            .mapped(tx, ProductTemplateAttributeLine::VALUE_IDS)
            .await?;
        for (index, active) in actives.into_iter().enumerate() {
            if active {
                let values = &all_values[index];
                if values.is_empty() {
                    return Err(Error::validation(
                        "The attribute line must contain at least one value.",
                    ));
                }
            }
        }
        Ok(())
    }

    /// Ensure every value belongs to the declared attribute.
    #[constrains("attribute_id", "value_ids")]
    async fn _check_value_attribute_match(
        this: &RecordSet<ProductTemplateAttributeLine>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let val_model = this.env().records("product.attribute.value")?;
        let attr_ids = this
            .mapped(tx, ProductTemplateAttributeLine::ATTRIBUTE_ID)
            .await?;
        let all_values = this
            .mapped(tx, ProductTemplateAttributeLine::VALUE_IDS)
            .await?;

        for (index, attr_opt) in attr_ids.into_iter().enumerate() {
            if let Some(attr_id) = attr_opt {
                let values = &all_values[index];
                for val_id in values {
                    let val_rec = val_model.browse(vec![val_id.get()])?;
                    let actual_attr = val_rec.cell(tx, "attribute_id").await?;
                    if let Cell::Id(actual_id) = actual_attr
                        && actual_id != attr_id.get()
                    {
                        return Err(Error::validation(
                            "The attribute of the value must match the line attribute.",
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    #[method(override)]
    async fn create(
        this: &RecordSet<ProductTemplateAttributeLine>,
        tx: &mut Transaction,
        vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let records = sup.call(tx, this.as_records(), (vals_list,)).await?;
        Self::sync_lines(
            this,
            tx,
            records.rec_ids().iter().map(|r| r.get()).collect(),
        )
        .await?;
        Ok(records)
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<ProductTemplateAttributeLine>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        sup.call(tx, this.as_records(), (vals,)).await?;
        Self::sync_lines(
            this,
            tx,
            this.as_records()
                .rec_ids()
                .iter()
                .map(|r| r.get())
                .collect(),
        )
        .await
    }
}

impl ProductTemplateAttributeLine {
    /// Materialize the PTAVs of every line first, then regenerate the variants once per template,
    /// so a batch of lines never creates the intermediate variant matrices.
    async fn sync_lines(
        this: &RecordSet<ProductTemplateAttributeLine>,
        tx: &mut Transaction,
        line_ids: Vec<i64>,
    ) -> Result<()> {
        let mut templates: Vec<i64> = Vec::new();
        for line_id in line_ids {
            crate::variant_generator::update_line_ptavs(tx, this.env(), line_id).await?;
            let line = this.browse(vec![line_id])?;
            if let Cell::Id(tmpl_id) = line.cell(tx, "product_tmpl_id").await?
                && !templates.contains(&tmpl_id)
            {
                templates.push(tmpl_id);
            }
        }
        for tmpl_id in templates {
            crate::variant_generator::sync_template_variants(tx, this.env(), tmpl_id).await?;
        }
        Ok(())
    }
}

/// `product.template.attribute.value`: Materialized join record with extra price and exclusions.
#[model(
    name = "product.template.attribute.value",
    description = "Product Template Attribute Value",
    order = "id",
    rec_name = "display_name",
    company_access = parent(edge = "product_tmpl_id"),
)]
pub struct ProductTemplateAttributeValue {
    /// Parent template.
    #[field(
        many2one,
        comodel = "product.template",
        required = true,
        ondelete = "cascade",
        index = true
    )]
    pub product_tmpl_id: Option<RecId>,

    /// Parent attribute line.
    #[field(
        many2one,
        comodel = "product.template.attribute.line",
        required = true,
        ondelete = "cascade",
        index = true
    )]
    pub attribute_line_id: Option<RecId>,

    /// Raw attribute value.
    #[field(
        many2one,
        comodel = "product.attribute.value",
        required = true,
        index = true
    )]
    pub product_attribute_value_id: Option<RecId>,

    /// Associated attribute.
    #[field(
        many2one,
        comodel = "product.attribute",
        index = true,
        string = "Attribute"
    )]
    pub attribute_id: Option<RecId>,

    /// Extra price added to variants having this value.
    #[field(float, digits = "Product Price")]
    pub price_extra: Decimal,

    /// Incompatible PTAVs on the same template (exclusion rules).
    #[field(
        many2many,
        comodel = "product.template.attribute.value",
        relation = "product_template_attribute_value_exclusion_rel",
        column1 = "product_template_attribute_value_id",
        column2 = "excluded_product_template_attribute_value_id"
    )]
    pub excluded_value_ids: Vec<RecId>,

    /// Display label.
    #[field(char, compute = "_compute_display_name")]
    pub display_name: Option<Arc<str>>,
}

#[methods(ProductTemplateAttributeValue, addon = "product")]
impl ProductTemplateAttributeValue {
    /// The
    /// attribute comes from the line, which always has one.
    #[compute(
        depends = [
            "product_attribute_value_id.name",
            "attribute_line_id.attribute_id.name"
        ],
        depends_context = ["display_attribute_name"]
    )]
    async fn _compute_display_name(
        this: &RecordSet<ProductTemplateAttributeValue>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ProductTemplateAttributeValue>,
    ) -> Result<()> {
        let with_attribute = this
            .env()
            .context()
            .get("display_attribute_name")
            .map(|v| v.is_truthy())
            .unwrap_or(true);
        let val_model = this.env().records("product.attribute.value")?;
        let line_model = this.env().records("product.template.attribute.line")?;
        let attr_model = this.env().records("product.attribute")?;
        let val_ids = this
            .mapped(
                tx,
                ProductTemplateAttributeValue::PRODUCT_ATTRIBUTE_VALUE_ID,
            )
            .await?;
        let line_ids = this
            .mapped(tx, ProductTemplateAttributeValue::ATTRIBUTE_LINE_ID)
            .await?;

        for (index, (val_opt, line_opt)) in val_ids.into_iter().zip(line_ids).enumerate() {
            let label = match val_opt {
                Some(val_id) => {
                    let value = match val_model
                        .browse(vec![val_id.get()])?
                        .cell(tx, "name")
                        .await?
                    {
                        Cell::Str(s) => s.to_string(),
                        _ => String::new(),
                    };
                    match (with_attribute, line_opt) {
                        (true, Some(line_id)) => {
                            let attr = line_model
                                .browse(vec![line_id.get()])?
                                .cell(tx, "attribute_id")
                                .await?;
                            let attribute = match attr {
                                Cell::Id(id) => {
                                    match attr_model.browse(vec![id])?.cell(tx, "name").await? {
                                        Cell::Str(s) => s.to_string(),
                                        _ => String::new(),
                                    }
                                }
                                _ => String::new(),
                            };
                            format!("{attribute}: {value}")
                        }
                        _ => value,
                    }
                }
                None => String::new(),
            };
            out.set(
                index,
                ProductTemplateAttributeValue::DISPLAY_NAME,
                Some(label.into()),
            )?;
        }
        Ok(())
    }
}
