// SPDX-License-Identifier: MIT OR Apache-2.0
//! Smart buttons that connect vendors (`res.partner`) and products (`product.template`)
//! directly to their readable confirmed purchase orders (`state = 'purchase'`), the same rule as
//! the sales buttons: RFQs and cancelled orders do not count. The vendor count is one grouped
//! read under the caller's read access; the product count follows the order lines, which no
//! single relation field of the order names, so it filters the linked order ids by read access.
//! Both actions open a domain, never an id list.

use erp_addon_account::smart_buttons::{domain_action, grouped_counts, leaf, the_one};
use erp_addon_base::ResPartner;
use erp_addon_product::ProductTemplate;
use erp_sdk::prelude::*;

use crate::bill_buttons::{counts, readable_links};

/// The state of the orders the buttons count: confirmed purchase orders.
const CONFIRMED: &str = "purchase";

/// Product template id, confirmed purchase order id.
const PRODUCT_PURCHASES: &str = "SELECT DISTINCT p.product_tmpl_id::int8, l.order_id::int8 \
    FROM purchase_order_line l \
    JOIN product_product p ON p.id = l.product_id \
    JOIN purchase_order o ON o.id = l.order_id \
    WHERE p.product_tmpl_id = ANY($1) AND o.state = 'purchase'";

/// `res.partner` with its readable purchase order count and vendor filter links.
#[extend(ResPartner, addon = "purchase")]
pub struct PartnerPurchaseOrders {
    /// Purchase orders placed with this vendor (used for vendor filtering).
    #[field(
        one2many,
        comodel = "purchase.order",
        inverse_name = "partner_id",
        groups = "purchase.group_purchase_user",
        string = "Purchase Orders"
    )]
    pub purchase_order_ids: Vec<RecId>,
    /// Vendor pricelist entries for this partner (used for vendor filtering).
    #[field(
        one2many,
        comodel = "product.supplierinfo",
        inverse_name = "partner_id",
        groups = "purchase.group_purchase_user",
        string = "Vendor Pricelists"
    )]
    pub supplierinfo_ids: Vec<RecId>,
    /// Readable confirmed purchase orders placed with this vendor.
    #[field(
        integer,
        compute = "_compute_purchase_order_count",
        string = "Purchase Orders"
    )]
    pub purchase_order_count: i32,
}

#[methods(ResPartner, addon = "purchase")]
impl PartnerPurchaseOrders {
    #[compute(depends = ["company_id"], depends_context = ["uid", "allowed_company_ids"])]
    async fn _compute_purchase_order_count(
        this: &RecordSet<ResPartner>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ResPartner>,
    ) -> Result<()> {
        let records = this.as_records();
        let confirmed = Domain::field("state")?.eq(CONFIRMED);
        let found = grouped_counts(tx, records, "purchase.order", "partner_id", confirmed).await?;
        for (index, count) in found.into_iter().enumerate() {
            out.set(index, PartnerPurchaseOrders::PURCHASE_ORDER_COUNT, count)?;
        }
        Ok(())
    }

    /// Open the confirmed purchase orders of this vendor.
    #[method]
    #[rpc(effect = read)]
    async fn action_view_purchase_orders(
        this: &RecordSet<ResPartner>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let id = the_one(this.as_records(), tx).await?;
        domain_action(
            tx,
            this.as_records(),
            "Purchase Orders",
            "purchase.order",
            Vec::new(),
            vec![
                Value::str("&"),
                leaf("partner_id", "=", Value::Int(id)),
                leaf("state", "=", Value::str(CONFIRMED)),
            ],
        )
        .await
    }
}

/// `product.template` with the count of purchase orders sourcing its variants.
#[extend(ProductTemplate, addon = "purchase")]
pub struct ProductTemplatePurchases {
    /// Readable confirmed purchase orders containing this product.
    #[field(
        integer,
        compute = "_compute_purchase_order_count",
        string = "Purchase Count"
    )]
    pub purchase_order_count: i32,
}

#[methods(ProductTemplate, addon = "purchase")]
impl ProductTemplatePurchases {
    #[compute(depends = ["company_id"], depends_context = ["uid", "allowed_company_ids"])]
    async fn _compute_purchase_order_count(
        this: &RecordSet<ProductTemplate>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ProductTemplate>,
    ) -> Result<()> {
        let records = this.as_records();
        let found = readable_links(tx, records, PRODUCT_PURCHASES, "purchase.order").await?;
        for (index, count) in counts(records, &found)?.into_iter().enumerate() {
            out.set(index, ProductTemplatePurchases::PURCHASE_ORDER_COUNT, count)?;
        }
        Ok(())
    }

    /// Open the confirmed purchase orders sourcing this product.
    #[method]
    #[rpc(effect = read)]
    async fn action_view_purchases(
        this: &RecordSet<ProductTemplate>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let id = the_one(this.as_records(), tx).await?;
        domain_action(
            tx,
            this.as_records(),
            "Purchase Orders",
            "purchase.order",
            Vec::new(),
            vec![
                Value::str("&"),
                leaf("order_line.product_id.product_tmpl_id", "=", Value::Int(id)),
                leaf("state", "=", Value::str(CONFIRMED)),
            ],
        )
        .await
    }
}
