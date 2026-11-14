// SPDX-License-Identifier: MIT OR Apache-2.0
//! Inventory feature identities compose with the base and catalog light-group classification.

use erp_addon_base::ResGroups;
use erp_sdk::prelude::*;

#[extend(ResGroups, addon = "stock")]
struct StockGroups {}

#[methods(ResGroups, addon = "stock")]
impl StockGroups {
    #[method(override)]
    async fn _get_light_group_xmlids(
        this: &RecordSet<ResGroups>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<Vec<String>> {
        let mut groups: Vec<String> = sup.call(tx, this.as_records(), ()).await?;
        // Structural group identities, not configurable policy values. Administrators edit
        // memberships and implications on the group records; this hook classifies feature groups.
        for xmlid in [
            "stock.group_production_lot",
            "stock.group_adv_location",
            "stock.group_stock_multi_locations",
            "stock.group_stock_picking_batch",
            "stock.group_tracking_lot",
            "stock.group_tracking_owner",
            "stock.group_lot_on_delivery_slip",
            "stock.group_stock_lot_print_gs1",
            "stock.group_stock_sign_delivery",
            "stock.group_warning_stock",
        ] {
            if !groups.iter().any(|id| id == xmlid) {
                groups.push(xmlid.to_owned());
            }
        }
        Ok(groups)
    }

    /// Reclassify after the manifest has published the inventory group identities, including
    /// upgrades. This installation service exposes no RPC method and grants no permissions.
    #[method]
    #[data_callable(model)]
    async fn _stock_reclassify_light_groups(
        this: &RecordSet<ResGroups>,
        tx: &mut Transaction,
    ) -> Result<()> {
        this.as_records()
            .call::<(), ()>(tx, "_apply_group_regular", ())
            .await
    }
}
