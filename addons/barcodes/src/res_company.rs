// SPDX-License-Identifier: MIT OR Apache-2.0
//! Company selection is durable and retains the existing company authorization policy.
use crate::models::default_nomenclature;
use erp_addon_base::ResCompany;
use erp_sdk::ValueMap;
use erp_sdk::prelude::*;

#[extend(ResCompany, addon = "barcodes")]
struct BarcodeCompany {
    #[field(
        many2one,
        comodel = "barcode.nomenclature",
        ondelete = "set null",
        index = true
    )]
    pub nomenclature_id: Option<RecId>,
}
#[methods(ResCompany, addon = "barcodes")]
impl BarcodeCompany {
    #[method(override)]
    async fn default_get(
        this: &RecordSet<ResCompany>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<ValueMap> {
        let requested = names.iter().any(|name| name == "nomenclature_id");
        let mut result: ValueMap = sup.call(tx, this.as_records(), (names,)).await?;
        if requested
            && !result.contains_key("nomenclature_id")
            && let Some(id) = default_nomenclature(tx, this.env(), false).await?
        {
            result.insert("nomenclature_id".into(), Value::Int(id));
        }
        Ok(result)
    }
    #[method(override)]
    async fn create(
        this: &RecordSet<ResCompany>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        if vals_list
            .iter()
            .any(|vals| !vals.contains("nomenclature_id"))
            && let Some(id) = default_nomenclature(tx, this.env(), false).await?
        {
            for vals in &mut vals_list {
                if !vals.contains("nomenclature_id") {
                    vals.insert("nomenclature_id".into(), Cell::Id(id));
                }
            }
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }
    /// Module data loader only. Runs after default records exist; never exposed over RPC.
    #[method]
    #[data_callable(model)]
    async fn _barcode_assign_defaults(
        this: &RecordSet<ResCompany>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let default = default_nomenclature(tx, this.env(), true)
            .await?
            .ok_or_else(|| Error::missing("Default barcode nomenclature is missing."))?;
        let companies = this
            .as_records()
            .with_context([("active_test", Value::Bool(false))])
            .search(
                tx,
                &Domain::condition("nomenclature_id", Operator::Eq, false)?,
            )
            .await?;
        companies
            .write(
                tx,
                RawVals::new().set_cell("nomenclature_id", Cell::Id(default)),
            )
            .await
    }
}
