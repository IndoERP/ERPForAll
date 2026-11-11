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
