//! Product addon typed error definitions.

use erp_sdk::Error;

/// Domain error for product catalog, variant generation, barcode collisions, and pricing.
#[derive(Debug, thiserror::Error)]
pub enum ProductError {
    #[error(
        "Product template {template_id} has impossible attribute configuration: no variants remain."
    )]
    ImpossibleConfiguration { template_id: i64 },

    #[error("Dynamic variant limit of {limit} exceeded for product template {template_id}.")]
    DynamicVariantLimitExceeded { template_id: i64, limit: i64 },

    #[error("A product or packaging already uses barcode '{barcode}' in this company.")]
    DuplicateBarcode { barcode: String },

    #[error("Cannot change create_variant mode: attribute is already used in {count} product(s).")]
    AttributeModeFrozen { count: usize },

    #[error("Supplierinfo variant {variant_id} does not belong to product template {template_id}.")]
    SupplierinfoVariantMismatch { variant_id: i64, template_id: i64 },

    #[error(
        "Cannot archive product {product_id} because it is configured as a component in combo choices."
    )]
    ComboArchivingForbidden { product_id: i64 },

    #[error("Pricelist recursion detected while evaluating pricelist {pricelist_id}.")]
    PricelistRecursion { pricelist_id: i64 },
}

impl From<ProductError> for Error {
    fn from(err: ProductError) -> Self {
        match err {
            ProductError::DuplicateBarcode { .. }
            | ProductError::SupplierinfoVariantMismatch { .. }
            | ProductError::AttributeModeFrozen { .. } => Error::validation(err.to_string()),
            ProductError::ImpossibleConfiguration { .. }
            | ProductError::DynamicVariantLimitExceeded { .. }
            | ProductError::ComboArchivingForbidden { .. }
            | ProductError::PricelistRecursion { .. } => Error::user(err.to_string()),
        }
    }
}
