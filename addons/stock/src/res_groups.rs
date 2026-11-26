// SPDX-License-Identifier: MIT OR Apache-2.0
//! Inventory feature identities compose with the base and catalog light-group classification.

use erp_addon_base::ResGroups;
use erp_sdk::prelude::*;

#[extend(ResGroups, addon = "stock")]
struct StockGroups {}

async fn is_multi_locations_enabled(tx: &mut Transaction) -> Result<bool> {
    let rows = tx
        .query(
            "SELECT EXISTS (SELECT 1 FROM res_groups_implied_rel r \
             JOIN ir_model_data u ON u.model = 'res.groups' AND u.module = 'base' \
              AND u.name = 'group_user' AND u.res_id = r.gid \
             JOIN ir_model_data m ON m.model = 'res.groups' AND m.module = 'stock' \
              AND m.name = 'group_stock_multi_locations' AND m.res_id = r.hid)",
            &[],
        )
        .await?;
    rows.first()
        .ok_or_else(|| Error::internal("Multi-location group implication check returned no row."))?
        .try_get(0)
        .map_err(|e| {
            Error::db("Failed to read multi-location group implication state.").with_source(e)
        })
}

#[methods(ResGroups, addon = "stock")]
impl StockGroups {
    #[method(override)]
    async fn write(
        this: &RecordSet<ResGroups>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        let implied_changed = vals.contains("implied_ids")
            || vals.commands("implied_ids").is_some()
            || vals.contains("implied_by_ids")
            || vals.commands("implied_by_ids").is_some();
        let was_enabled = if implied_changed {
            tx.flush().await?;
            is_multi_locations_enabled(tx).await?
        } else {
            false
        };
        sup.call(tx, this.as_records(), (vals,)).await?;
        if implied_changed && !was_enabled {
            tx.flush().await?;
            // Only on the false -> true transition of Multiple Locations on base.group_user,
            // activate active warehouses' Internal Moves operation types so Internal Transfers
            // become selectable without overriding later manual archiving.
            if is_multi_locations_enabled(tx).await? {
                tx.execute(
                    "UPDATE stock_picking_type pt \
                     SET active = true \
                     FROM stock_warehouse w \
                     WHERE w.int_type_id = pt.id AND w.active IS TRUE AND pt.active IS NOT TRUE",
                    &[],
                )
                .await?;
                tx.invalidate_model("stock.picking.type", Some(&["active"]))
                    .await?;
            }
        }
        Ok(())
    }

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
