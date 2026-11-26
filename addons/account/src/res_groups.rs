// SPDX-License-Identifier: MIT OR Apache-2.0
//! Accounting optional feature identities compose with the base light-group classification.
//! Each toggle starts disabled for SME/UMKM simplicity and is turned on from Accounting Settings.

use erp_addon_base::ResGroups;
use erp_sdk::prelude::*;

#[extend(ResGroups, addon = "account")]
struct AccountGroups {}

#[methods(ResGroups, addon = "account")]
impl AccountGroups {
    #[method(override)]
    async fn _get_light_group_xmlids(
        this: &RecordSet<ResGroups>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<Vec<String>> {
        let mut groups: Vec<String> = sup.call(tx, this.as_records(), ()).await?;
        for xmlid in [
            crate::COST_CENTER_GROUP,
            crate::ASSET_CIP_GROUP,
            crate::BUDGET_AVC_GROUP,
            crate::CONSOLIDATION_GROUP,
        ] {
            if !groups.iter().any(|id| id == xmlid) {
                groups.push(xmlid.to_owned());
            }
        }
        Ok(groups)
    }

    /// Reclassify after the manifest has published the accounting feature group identities,
    /// including upgrades. Exposes no RPC method and grants no permissions.
    #[method]
    #[data_callable(model)]
    async fn _account_reclassify_light_groups(
        this: &RecordSet<ResGroups>,
        tx: &mut Transaction,
    ) -> Result<()> {
        this.as_records()
            .call::<(), ()>(tx, "_apply_group_regular", ())
            .await
    }
}
