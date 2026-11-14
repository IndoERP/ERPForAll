// SPDX-License-Identifier: MIT OR Apache-2.0
//! Add the installed UoM feature classification to the ordinary light-group extension chain.
use erp_addon_base::ResGroups;
use erp_sdk::prelude::*;

#[extend(ResGroups, addon = "uom")]
struct UomGroups {}

#[methods(ResGroups, addon = "uom")]
impl UomGroups {
    #[method(override)]
    async fn _get_light_group_xmlids(
        this: &RecordSet<ResGroups>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<Vec<String>> {
        let mut groups: Vec<String> = sup.call(tx, this.as_records(), ()).await?;
        if !groups.iter().any(|id| id == "uom.group_uom") {
            groups.push("uom.group_uom".to_owned());
        }
        Ok(groups)
    }

    /// Global multi-unit feature state follows the designated superuser's effective groups.
    /// No current-caller membership or elevation flag substitutes for the feature setting.
    #[method]
    async fn _is_uom_feature_enabled(
        this: &RecordSet<ResGroups>,
        tx: &mut Transaction,
    ) -> Result<bool> {
        // This is global feature configuration, not an authorization decision about the
        // caller. Read the installed group's implication closure and user 1's direct links
        // under metadata elevation, retaining the caller as actor. Do not switch principals.
        let metadata = this.env().sudo(true);
        let data = metadata
            .records("ir.model.data")?
            .search(
                tx,
                &(Domain::condition("module", Operator::Eq, "uom")?
                    & Domain::condition("name", Operator::Eq, "group_uom")?
                    & Domain::condition("model", Operator::Eq, "res.groups")?),
            )
            .await?;
        let group_id = match data.cell(tx, "res_id").await? {
            Cell::Id(id) => id,
            _ => {
                return Err(Error::programming(
                    "The installed UoM feature group identity is missing or invalid.",
                ));
            }
        };
        let group = metadata.records("res.groups")?.browse([group_id])?;
        let implied_by = group.related(tx, "all_implied_by_ids").await?.ids();
        let root = metadata
            .records("res.users")?
            .browse([erp_sdk::SUPERUSER_ID.get()])?;
        let direct = root.related(tx, "group_ids").await?.ids();
        Ok(direct.iter().any(|id| implied_by.contains(id)))
    }

    /// Rebalance only after data loading has published the feature group's XML identity.
    /// Data-callable for the install/upgrade file; not exposed as an RPC method.
    #[method]
    #[data_callable(model)]
    async fn _uom_reclassify_light_groups(
        this: &RecordSet<ResGroups>,
        tx: &mut Transaction,
    ) -> Result<()> {
        this.as_records()
            .call::<(), ()>(tx, "_apply_group_regular", ())
            .await
    }
}
