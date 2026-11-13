//! Product master catalog, variants, categories, pricelists, and vendor agreements.

erp_sdk::addon!(
    name = "product",
    exact_only = true,
    depends = ["base", "uom"]
);

pub mod attribute;
pub mod barcode;
pub mod category;
pub mod combo;
pub mod defaults;
pub mod error;
pub mod migrations;
mod names;
pub mod pricelist;
pub mod product;
pub mod product_uom;
pub mod sellers;
pub mod supplierinfo;
pub mod template;
pub mod template_attribute;
pub mod variant_generator;

pub use attribute::{ProductAttribute, ProductAttributeValue};
pub use category::ProductCategory;
pub use combo::{ProductCombo, ProductComboItem};
pub use pricelist::{ProductPricelist, ProductPricelistItem};
pub use product::{ProductProduct, ProductProductMethods};
pub use product_uom::{ProductUom, UomUomPackagings};
pub use sellers::{ProductProductMethods as ProductSellerMethods, SellerInfo, SellerQuery};
pub use supplierinfo::ProductSupplierinfo;
pub use template::{ProductTemplate, ProductTemplateMethods};
pub use template_attribute::{ProductTemplateAttributeLine, ProductTemplateAttributeValue};
