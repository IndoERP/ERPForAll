// SPDX-License-Identifier: MIT OR Apache-2.0
//! Classify catalog feature groups through the existing base/UoM extension chain.

use erp_addon_base::ResGroups;
use erp_sdk::prelude::*;

#[extend(ResGroups, addon = "product")]
struct ProductGroups {}

#[methods(ResGroups, addon = "product")]
impl ProductGroups {
    #[method(override)]
    async fn _get_light_group_xmlids(
        this: &RecordSet<ResGroups>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<Vec<String>> {
        let mut groups: Vec<String> = sup.call(tx, this.as_records(), ()).await?;
        for xmlid in [
            "product.group_product_pricelist",
            "product.group_product_variant",
        ] {
            if !groups.iter().any(|id| id == xmlid) {
                groups.push(xmlid.to_owned());
            }
        }
        Ok(groups)
    }

    /// Run after catalog security data publishes the group identities, including upgrades.
    /// This is an installation service, not an RPC entry point or an access grant.
    #[method]
    #[data_callable(model)]
    async fn _product_reclassify_light_groups(
        this: &RecordSet<ResGroups>,
        tx: &mut Transaction,
    ) -> Result<()> {
        this.as_records()
            .call::<(), ()>(tx, "_apply_group_regular", ())
            .await
    }
}
