//! Product mail bridge: extends `product.template` and `product.product` with `mail.thread`.

erp_sdk::addon!(name = "product_mail", depends = ["base", "product", "mail"]);

use erp_addon_mail::MailThread;
use erp_addon_product::{ProductProduct, ProductTemplate};
use erp_sdk::Operation;
use erp_sdk::prelude::*;

/// `product.template` with the thread mixin's messages, followers and follower partners.
#[extend(ProductTemplate, addon = "product_mail", inherit = [MailThread])]
pub struct ProductTemplateThread {}

#[methods(ProductTemplate, addon = "product_mail")]
impl ProductTemplateThread {
    /// Posting on a product template needs read access.
    #[method(override)]
    async fn _mail_post_access(
        _this: &RecordSet<ProductTemplate>,
        _tx: &mut Transaction,
        _sup: Super,
    ) -> Result<Operation> {
        Ok(Operation::Read)
    }
}

/// `product.product` with the thread mixin's messages, followers and follower partners.
#[extend(ProductProduct, addon = "product_mail", inherit = [MailThread])]
pub struct ProductProductThread {}

#[methods(ProductProduct, addon = "product_mail")]
impl ProductProductThread {
    /// Posting on a product variant needs read access.
    #[method(override)]
    async fn _mail_post_access(
        _this: &RecordSet<ProductProduct>,
        _tx: &mut Transaction,
        _sup: Super,
    ) -> Result<Operation> {
        Ok(Operation::Read)
    }
}
