// SPDX-License-Identifier: MIT OR Apache-2.0
//! Product Attribute models (`product.attribute`, `product.attribute.value`).

use erp_sdk::prelude::*;
use erp_sdk::{Operation, ValueMap};
use std::collections::BTreeSet;

/// `product.attribute`: Defines a product attribute (e.g. Size, Color).
#[model(
    name = "product.attribute",
    description = "Product Attribute",
    order = "sequence, id",
    rec_name = "name",
    constraints = [(
        "_check_multi_checkbox_no_variant",
        "CHECK(display_type != 'multi' OR create_variant = 'no_variant')",
        "The display type 'multi' can only be used with the 'Never (no_variant)' variant creation mode."
    )]
)]
pub struct ProductAttribute {
    /// Attribute name.
    #[field(char, required = true, translate = true)]
    pub name: Option<Arc<str>>,

    /// Sequence order.
    #[field(integer, default = 20)]
    pub sequence: i32,

    /// Variant creation mode: 'always', 'dynamic', 'no_variant'.
    #[field(
        selection,
        selection = [
            ("always", "Instantly"),
            ("dynamic", "Dynamically"),
            ("no_variant", "Never")
        ],
        default = "always",
        required = true
    )]
    pub create_variant: Option<Arc<str>>,

    /// Visual selection widget type in forms / configurator.
    #[field(
        selection,
        selection = [
            ("radio", "Radio"),
            ("pills", "Pills"),
            ("select", "Select"),
            ("color", "Color"),
            ("multi", "Multi-checkbox"),
            ("image", "Image"),
            ("range", "Range")
        ],
        default = "radio",
        required = true
    )]
    pub display_type: Option<Arc<str>>,

    /// Permissible values for this attribute.
    #[field(
        one2many,
        comodel = "product.attribute.value",
        inverse_name = "attribute_id"
    )]
    pub value_ids: Vec<RecId>,

    /// Hide an unused attribute without deleting its identity.
    #[field(boolean, default = true)]
    pub active: bool,

    /// Materialized values belonging to this attribute.
    #[field(
        one2many,
        comodel = "product.template.attribute.value",
        inverse_name = "attribute_id"
    )]
    pub template_value_ids: Vec<RecId>,

    /// Template configuration lines.
    #[field(
        one2many,
        comodel = "product.template.attribute.line",
        inverse_name = "attribute_id"
    )]
    pub attribute_line_ids: Vec<RecId>,

    /// Templates aggregated from active lines, including archived templates.
    #[field(many2many, comodel = "product.template", compute = "_compute_products")]
    pub product_tmpl_ids: Vec<RecId>,

    /// Number of lines belonging to active product templates.
    #[field(integer, compute = "_compute_number_related_products")]
    pub number_related_products: i32,
}

#[methods(ProductAttribute, addon = "product")]
impl ProductAttribute {
    #[compute(depends = ["attribute_line_ids.active", "attribute_line_ids.product_tmpl_id", "attribute_line_ids.product_tmpl_id.active"])]
    async fn _compute_number_related_products(
        this: &RecordSet<ProductAttribute>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ProductAttribute>,
    ) -> Result<()> {
        for (index, id) in this.rec_ids().iter().enumerate() {
            let domain = Domain::condition("attribute_id", Operator::Eq, id.get())?
                & Domain::condition("product_tmpl_id.active", Operator::Eq, true)?;
            let count = this
                .env()
                .records("product.template.attribute.line")?
                .search_count(tx, &domain, None)
                .await?;
            out.set(
                index,
                ProductAttribute::NUMBER_RELATED_PRODUCTS,
                i32::try_from(count)
                    .map_err(|_| Error::programming("Related product line count exceeds i32."))?,
            )?;
        }
        Ok(())
    }

    #[compute(depends = ["attribute_line_ids.active", "attribute_line_ids.product_tmpl_id"])]
    async fn _compute_products(
        this: &RecordSet<ProductAttribute>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ProductAttribute>,
    ) -> Result<()> {
        for (index, id) in this.rec_ids().iter().enumerate() {
            let lines = this
                .env()
                .records("product.template.attribute.line")?
                .with_context([("active_test", Value::Bool(true))])
                .search(
                    tx,
                    &Domain::condition("attribute_id", Operator::Eq, id.get())?,
                )
                .await?;
            let mut templates = BTreeSet::new();
            for line in lines.rec_ids() {
                let row = lines.browse_ids([*line]);
                if let Cell::Id(tmpl) = row.cell(tx, "product_tmpl_id").await? {
                    templates.insert(
                        RecId::from_raw(tmpl)
                            .ok_or_else(|| Error::programming("Invalid template identity."))?,
                    );
                } else {
                    return Err(Error::programming(
                        "Attribute line has no product template.",
                    ));
                }
            }
            out.set(
                index,
                ProductAttribute::PRODUCT_TMPL_IDS,
                templates.into_iter().collect::<Vec<_>>(),
            )?;
        }
        Ok(())
    }

    #[onchange("display_type")]
    async fn _onchange_display_type(
        this: &RecordSet<ProductAttribute>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        for id in this.rec_ids() {
            let row = this.as_records().browse_ids([*id]);
            if row.cell(tx, "display_type").await? == Cell::Str("multi".into())
                && row.cell(tx, "number_related_products").await? == Cell::Int(0)
            {
                row.write_base(
                    tx,
                    RawVals::new().set_cell("create_variant", Cell::Str("no_variant".into())),
                )
                .await?;
            }
        }
        Ok(OnchangeResult::none())
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<ProductAttribute>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        this.as_records().check_access(tx, Operation::Write).await?;
        for id in this.rec_ids() {
            let row = this.as_records().browse_ids([*id]);
            if let Some(mode) = vals.get("create_variant")
                && *mode != row.cell(tx, "create_variant").await?
            {
                refuse_attribute_use(tx, &row, "change variant creation mode for").await?;
            }
            if vals.get("active") == Some(&Cell::Bool(false)) {
                let used = attribute_usage(tx, &row).await?;
                if !used.is_empty() {
                    return Err(Error::user(
                        "Products still use this attribute, so it cannot be archived. Remove it from those products first.",
                    ));
                }
            }
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }

    #[method(override)]
    async fn unlink(
        this: &RecordSet<ProductAttribute>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        this.as_records()
            .check_access(tx, Operation::Unlink)
            .await?;
        for id in this.rec_ids() {
            refuse_attribute_use(tx, &this.as_records().browse_ids([*id]), "delete").await?;
        }
        sup.call(tx, this.as_records(), ()).await
    }
}

// Global attributes affect every company. Guards see hidden use, while diagnostic names use
// the caller's ordinary visibility. An inaccessible product never leaks through the message.
async fn attribute_usage(tx: &mut Transaction, row: &Records) -> Result<Records> {
    let domain = Domain::condition(
        "attribute_id",
        Operator::Eq,
        row.ensure_one()?.rec_ids()[0].get(),
    )? & Domain::condition("product_tmpl_id.active", Operator::Eq, true)?;
    row.env()
        .sudo(true)
        .records("product.template.attribute.line")?
        .with_context([("active_test", Value::Bool(true))])
        .search(tx, &domain)
        .await
}

async fn refuse_attribute_use(tx: &mut Transaction, row: &Records, operation: &str) -> Result<()> {
    let lines = attribute_usage(tx, row).await?;
    if !lines.is_empty() {
        let label = required_text(row.cell(tx, "name").await?, "attribute.name")?;
        let names = visible_product_names(tx, row.env(), &lines).await?;
        return Err(Error::user(format!(
            "Cannot {operation} attribute {label} while active products use it: {names}."
        )));
    }
    Ok(())
}

async fn visible_product_names(tx: &mut Transaction, env: &Env, lines: &Records) -> Result<String> {
    let templates = lines.related(tx, "product_tmpl_id").await?;
    let visible = env
        .records("product.template")?
        .with_context([("active_test", Value::Bool(false))])
        .search(tx, &Domain::condition("id", Operator::In, templates.ids())?)
        .await?;
    let mut names = BTreeSet::new();
    for id in visible.rec_ids() {
        names.insert(required_text(
            visible.browse_ids([*id]).cell(tx, "display_name").await?,
            "product.template.display_name",
        )?);
    }
    if visible.len() < templates.len() {
        names.insert("restricted products".to_owned());
    }
    Ok(names.into_iter().collect::<Vec<_>>().join(", "))
}

fn required_text(value: Cell, field: &str) -> Result<String> {
    match value {
        Cell::Str(text) => Ok(text.to_string()),
        other => Err(Error::programming(format!(
            "{field} returned {other:?}, expected text."
        ))),
    }
}

/// `product.attribute.value`: A specific value for an attribute (e.g. Red, XL).
#[model(
    name = "product.attribute.value",
    description = "Attribute Value",
    order = "attribute_id, sequence, id",
    rec_name = "name"
)]
pub struct ProductAttributeValue {
    /// Value label.
    #[field(char, required = true, translate = true)]
    pub name: Option<Arc<str>>,

    /// Sequence order.
    #[field(integer)]
    pub sequence: i32,

    /// Parent attribute.
    #[field(
        many2one,
        comodel = "product.attribute",
        required = true,
        ondelete = "cascade",
        index = true
    )]
    pub attribute_id: Option<RecId>,

    /// Default extra price copied to PTAV lines.
    #[field(float, digits = "Product Price", default = 0)]
    pub default_extra_price: Decimal,

    /// User-editable palette choice (default sampled from the reference palette).
    #[field(integer)]
    pub color: i32,

    /// Free-text option marker for configurators.
    #[field(boolean, default = false)]
    pub is_custom: bool,
    /// User-selected HTML color.
    #[field(char)]
    pub html_color: Option<Arc<str>>,
    /// Search/archive state, independent of variant activity.
    #[field(boolean, default = true)]
    pub active: bool,
    /// Owning attribute's display choice.
    #[field(selection, related = "attribute_id.display_type")]
    pub display_type: Option<Arc<str>>,
    /// Configuration lines that select this value; never copied with the value.
    #[field(
        many2many,
        comodel = "product.template.attribute.line",
        relation = "product_template_attribute_line_value_rel",
        column1 = "product_attribute_value_id",
        column2 = "product_template_attribute_line_id",
        copy = false
    )]
    pub pav_attribute_line_ids: Vec<RecId>,
    /// Context-sensitive label (`show_attribute`).
    #[field(char, compute = "_compute_display_name")]
    pub display_name: Option<Arc<str>>,
    /// Whether an active product template selects this value.
    #[field(boolean, compute = "_compute_is_used_on_products")]
    pub is_used_on_products: bool,
    /// Whether a draft default or an allowed-company materialized extra differs.
    #[field(boolean, compute = "_compute_default_extra_price_changed")]
    pub default_extra_price_changed: bool,
}

#[methods(ProductAttributeValue, addon = "product")]
impl ProductAttributeValue {
    #[method(override)]
    async fn default_get(
        this: &RecordSet<ProductAttributeValue>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<ValueMap> {
        let requested = names.iter().any(|name| name == "color");
        let mut defaults = sup.call(tx, this.as_records(), (names,)).await?;
        if requested && !defaults.contains_key("color") {
            // Palette indices are a display-format domain, not an operator admission limit.
            // Explicit/context/persisted defaults keep priority.
            let row = tx
                .query("SELECT floor(random() * 11)::int4 + 1", &[])
                .await?
                .into_iter()
                .next()
                .ok_or_else(|| {
                    Error::programming("Attribute value color default returned no row.")
                })?;
            let color: i32 = row.try_get(0).map_err(|e| {
                Error::db("Attribute value color default decode failed.").with_source(e)
            })?;
            defaults.insert("color".into(), Value::Int(i64::from(color)));
        }
        Ok(defaults)
    }

    #[compute(depends = ["name", "attribute_id.name"], depends_context = ["show_attribute"])]
    async fn _compute_display_name(
        this: &RecordSet<ProductAttributeValue>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ProductAttributeValue>,
    ) -> Result<()> {
        // Missing switch means include the attribute.
        let show = crate::names::context_flag(this.env(), "show_attribute", true)?;
        for (index, id) in this.rec_ids().iter().enumerate() {
            let row = this.as_records().browse_ids([*id]);
            let name = match row.cell(tx, "name").await? {
                Cell::Str(name) => name.to_string(),
                Cell::Null if id.is_new() => String::new(),
                other => {
                    return Err(Error::programming(format!(
                        "Attribute value name returned {other:?}, expected text."
                    )));
                }
            };
            let attr = row.related(tx, "attribute_id").await?;
            let label = if show && !attr.is_empty() {
                format!(
                    "{}: {name}",
                    required_text(attr.cell(tx, "name").await?, "attribute.name")?
                )
            } else {
                name
            };
            out.set(
                index,
                ProductAttributeValue::DISPLAY_NAME,
                Some(Arc::<str>::from(label)),
            )?;
        }
        Ok(())
    }

    #[compute(depends = ["pav_attribute_line_ids", "pav_attribute_line_ids.active", "pav_attribute_line_ids.product_tmpl_id.active"])]
    async fn _compute_is_used_on_products(
        this: &RecordSet<ProductAttributeValue>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ProductAttributeValue>,
    ) -> Result<()> {
        for (index, id) in this.rec_ids().iter().enumerate() {
            let lines = value_usage(tx, &this.as_records().browse_ids([*id]), false).await?;
            out.set(
                index,
                ProductAttributeValue::IS_USED_ON_PRODUCTS,
                !lines.is_empty(),
            )?;
        }
        Ok(())
    }

    #[compute(depends = ["default_extra_price", "pav_attribute_line_ids.product_template_value_ids.price_extra", "pav_attribute_line_ids.product_tmpl_id.company_id"], depends_context = ["allowed_company_ids"])]
    async fn _compute_default_extra_price_changed(
        this: &RecordSet<ProductAttributeValue>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ProductAttributeValue>,
    ) -> Result<()> {
        let companies: Vec<i64> = tx
            .companies(this.env())
            .await?
            .iter()
            .map(|id| id.get())
            .collect();
        let company_domain = Domain::condition("product_tmpl_id.company_id", Operator::Eq, false)?
            | Domain::condition("product_tmpl_id.company_id", Operator::ParentOf, companies)?;
        for (index, id) in this.rec_ids().iter().enumerate() {
            let row = this.as_records().browse_ids([*id]);
            let price = price_cell(row.cell(tx, "default_extra_price").await?)?;
            let origin = row.origin();
            let original_price = if origin.is_empty() {
                Decimal::ZERO
            } else {
                price_cell(origin.cell(tx, "default_extra_price").await?)?
            };
            let mut changed = id.is_new() && price != original_price;
            if let Some(origin_id) = origin.rec_ids().first() {
                // Only this boolean crosses elevation; company scope is validated beforehand.
                let domain =
                    Domain::condition("product_attribute_value_id", Operator::Eq, origin_id.get())?
                        & company_domain.clone();
                let ptavs = this
                    .env()
                    .sudo(true)
                    .records("product.template.attribute.value")?
                    .search(tx, &domain)
                    .await?;
                for ptav in ptavs.rec_ids() {
                    if price_cell(ptavs.browse_ids([*ptav]).cell(tx, "price_extra").await?)?
                        != price
                    {
                        changed = true;
                    }
                }
            }
            out.set(
                index,
                ProductAttributeValue::DEFAULT_EXTRA_PRICE_CHANGED,
                changed,
            )?;
        }
        Ok(())
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<ProductAttributeValue>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        this.as_records().check_access(tx, Operation::Write).await?;
        if let Some(attribute) = vals.get("attribute_id") {
            for id in this.rec_ids() {
                let row = this.as_records().browse_ids([*id]);
                if *attribute != row.cell(tx, "attribute_id").await? {
                    refuse_value_use(tx, &row, "change attribute for").await?;
                }
            }
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }

    #[method(override)]
    async fn unlink(
        this: &RecordSet<ProductAttributeValue>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        this.as_records()
            .check_access(tx, Operation::Unlink)
            .await?;
        let mut remove = Vec::new();
        let mut archive = Vec::new();
        for id in this.rec_ids() {
            let row = this.as_records().browse_ids([*id]);
            refuse_value_use(tx, &row, "delete").await?;
            let ptavs = this
                .env()
                .sudo(true)
                .records("product.template.attribute.value")?
                .search(
                    tx,
                    &Domain::condition("product_attribute_value_id", Operator::Eq, id.get())?,
                )
                .await?;
            let variants = this
                .env()
                .sudo(true)
                .records("product.product")?
                .with_context([("active_test", Value::Bool(false))])
                .search(
                    tx,
                    &Domain::condition(
                        "product_template_attribute_value_ids",
                        Operator::In,
                        ptavs.ids(),
                    )?,
                )
                .await?;
            let mut active = false;
            for variant in variants.rec_ids() {
                match variants.browse_ids([*variant]).cell(tx, "active").await? {
                    Cell::Bool(value) => active |= value,
                    other => {
                        return Err(Error::programming(format!(
                            "Variant activity returned {other:?}, expected boolean."
                        )));
                    }
                }
            }
            if !variants.is_empty() && !active {
                archive.push(*id);
            } else {
                remove.push(*id);
            }
        }
        // All business refusals precede mutation; the ordinary unlink chain handles FK errors.
        this.as_records()
            .browse_ids(archive)
            .action_archive(tx)
            .await?;
        let deleting = this.as_records().browse_ids(remove);
        sup.call(tx, &deleting, ()).await
    }
}

async fn value_usage(tx: &mut Transaction, row: &Records, global: bool) -> Result<Records> {
    let env = if global {
        row.env().sudo(true)
    } else {
        row.env().clone()
    };
    let domain = Domain::condition("value_ids", Operator::In, row.ids())?
        & Domain::condition("product_tmpl_id.active", Operator::Eq, true)?;
    env.records("product.template.attribute.line")?
        .search(tx, &domain)
        .await
}

async fn refuse_value_use(tx: &mut Transaction, row: &Records, operation: &str) -> Result<()> {
    let lines = value_usage(tx, row, true).await?;
    if !lines.is_empty() {
        let label = required_text(
            row.cell(tx, "display_name").await?,
            "attribute.value.display_name",
        )?;
        let products = visible_product_names(tx, row.env(), &lines).await?;
        return Err(Error::user(format!(
            "Cannot {operation} value {label} while active products use it: {products}."
        )));
    }
    Ok(())
}

fn price_cell(value: Cell) -> Result<Decimal> {
    match value {
        Cell::Decimal(price) => Ok(price),
        // Nullable legacy float fields read as zero; this is their declared numeric default.
        Cell::Null => Ok(Decimal::ZERO),
        other => Err(Error::programming(format!(
            "Attribute price returned {other:?}, expected decimal."
        ))),
    }
}
