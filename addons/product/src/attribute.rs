//! Product Attribute models (`product.attribute`, `product.attribute.value`).

use erp_sdk::prelude::*;

/// `product.attribute`: Defines a product attribute (e.g. Size, Color).
#[model(
    name = "product.attribute",
    description = "Product Attribute",
    order = "sequence, name, id",
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
    #[field(integer, default = 1)]
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
            ("multi", "Multi-checkbox")
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

    /// Count of product templates currently using this attribute.
    #[field(integer, compute = "_compute_number_related_products")]
    pub number_related_products: i32,
}

#[methods(ProductAttribute, addon = "product")]
impl ProductAttribute {
    /// Compute number of related product templates.
    #[compute(depends = ["value_ids"])]
    async fn _compute_number_related_products(
        this: &RecordSet<ProductAttribute>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ProductAttribute>,
    ) -> Result<()> {
        let ptal = this.env().records("product.template.attribute.line")?;
        for (index, attr_id) in this.as_records().rec_ids().iter().enumerate() {
            let domain = Domain::condition("attribute_id", Operator::Eq, attr_id.get())?;
            let count = ptal.search_count(tx, &domain, None).await?;
            let count = i32::try_from(count).map_err(|_| {
                Error::programming(format!("related product count {count} does not fit an i32"))
            })?;
            out.set(index, ProductAttribute::NUMBER_RELATED_PRODUCTS, count)?;
        }
        Ok(())
    }

    /// Freeze create_variant mode if attribute is already assigned to templates.
    #[constrains("create_variant")]
    async fn _check_create_variant_frozen(
        this: &RecordSet<ProductAttribute>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let counts = this
            .mapped(tx, ProductAttribute::NUMBER_RELATED_PRODUCTS)
            .await?;
        for count in counts {
            if count > 0 {
                return Err(Error::validation(
                    "You cannot change the variant creation mode of an attribute that is already used in products.",
                ));
            }
        }
        Ok(())
    }
}

/// `product.attribute.value`: A specific value for an attribute (e.g. Red, XL).
#[model(
    name = "product.attribute.value",
    description = "Attribute Value",
    order = "sequence, id",
    rec_name = "name"
)]
pub struct ProductAttributeValue {
    /// Value label.
    #[field(char, required = true, translate = true)]
    pub name: Option<Arc<str>>,

    /// Sequence order.
    #[field(integer, default = 1)]
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
    #[field(float, digits = "Product Price")]
    pub default_extra_price: Decimal,

    /// Optional color hex code or integer index for color swatch.
    #[field(integer, default = 0)]
    pub color: i32,
}
