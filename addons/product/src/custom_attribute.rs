// SPDX-License-Identifier: MIT OR Apache-2.0
//! User-entered text attached to a materialized attribute value.
use erp_sdk::prelude::*;

/// Custom text is a user-created record, never predefined catalog data. It
/// follows the company of its template value (BUG-146).
#[model(
    name = "product.attribute.custom.value",
    description = "Custom Attribute Value",
    order = "custom_product_template_attribute_value_id, id",
    rec_name = "name",
    company_access = parent(edge = "custom_product_template_attribute_value_id")
)]
pub struct ProductAttributeCustomValue {
    /// Label assembled from the referenced value and trimmed text.
    #[field(char, compute = "_compute_name")]
    pub name: Option<Arc<str>>,
    /// Stable referenced configuration; deleting it is refused while custom text exists. Indexed:
    /// every template-value delete checks it, and it leads the model order.
    #[field(
        many2one,
        comodel = "product.template.attribute.value",
        required = true,
        ondelete = "restrict",
        index = true
    )]
    pub custom_product_template_attribute_value_id: Option<RecId>,
    /// Text supplied by the owning user.
    #[field(char)]
    pub custom_value: Option<Arc<str>>,
}

#[methods(ProductAttributeCustomValue, addon = "product")]
impl ProductAttributeCustomValue {
    #[compute(depends = ["custom_product_template_attribute_value_id.display_name", "custom_value"])]
    async fn _compute_name(
        this: &RecordSet<ProductAttributeCustomValue>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ProductAttributeCustomValue>,
    ) -> Result<()> {
        for (index, id) in this.rec_ids().iter().enumerate() {
            let row = this.as_records().browse_ids([*id]);
            let text = match row.cell(tx, "custom_value").await? {
                Cell::Str(text) => text.trim().to_owned(),
                Cell::Null => String::new(), // Optional custom text has an empty label by contract.
                other => {
                    return Err(Error::programming(format!(
                        "Custom text returned {other:?}, expected text."
                    )));
                }
            };
            let ptav = row
                .related(tx, "custom_product_template_attribute_value_id")
                .await?;
            let label = if ptav.is_empty() {
                text
            } else {
                match ptav.cell(tx, "display_name").await? {
                    Cell::Str(name) if !name.is_empty() => format!("{name}: {text}"),
                    Cell::Str(_) | Cell::Null => text,
                    other => {
                        return Err(Error::programming(format!(
                            "Template value label returned {other:?}, expected text."
                        )));
                    }
                }
            };
            out.set(
                index,
                ProductAttributeCustomValue::NAME,
                Some(Arc::<str>::from(label)),
            )?;
        }
        Ok(())
    }
}
