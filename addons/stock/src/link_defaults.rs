// SPDX-License-Identifier: MIT OR Apache-2.0
//! Data-loader initialization of editable partner fallbacks. No provisioning or request-time sudo.
use crate::res_partner::StockPartner;
use erp_addon_base::ResPartner;
use erp_sdk::prelude::*;

#[methods(ResPartner, addon = "stock")]
impl StockPartner {
    /// Private module-data entry point, after the location records and field metadata exist.
    /// Repeat upgrades preserve administrator changes, including an explicit null default.
    #[method]
    #[data_callable(model)]
    async fn _stock_assign_link_defaults(
        this: &RecordSet<ResPartner>,
        tx: &mut Transaction,
    ) -> Result<()> {
        for (field, external_id) in [
            ("property_stock_customer", "stock.stock_location_customers"),
            ("property_stock_supplier", "stock.stock_location_suppliers"),
        ] {
            let key = erp_sdk::script::XmlId::parse(external_id).map_err(|e| {
                Error::programming("Invalid stock default external ID.").with_source(e)
            })?;
            let target = erp_sdk::script::env_ref(tx, this.env(), &key, true)
                .await?
                .ok_or_else(|| {
                    Error::missing(format!("Stock default {external_id} is missing."))
                })?;
            if target.model_name() != "stock.location" || target.exists(tx).await?.len() != 1 {
                return Err(Error::programming(format!(
                    "Stock default {external_id} must resolve to one stock.location record."
                )));
            }
            let target_id = target.ids().first().copied().ok_or_else(|| {
                Error::missing(format!("Stock default {external_id} has no record."))
            })?;
            let fields = this
                .env()
                .records("ir.model.fields")?
                .search(
                    tx,
                    &Domain::and([
                        Domain::condition("model", Operator::Eq, "res.partner")?,
                        Domain::condition("name", Operator::Eq, field)?,
                    ]),
                )
                .await?;
            if fields.len() != 1 {
                return Err(Error::programming(format!(
                    "Stock default requires one reflected res.partner.{field} field."
                )));
            }
            let defaults = this.env().records("ir.default")?;
            let existing = defaults
                .search(
                    tx,
                    &Domain::and([
                        Domain::condition("field_id", Operator::Eq, fields.ids()[0])?,
                        Domain::condition("user_id", Operator::Eq, false)?,
                        Domain::condition("company_id", Operator::Eq, false)?,
                        Domain::condition("condition", Operator::Eq, false)?,
                    ]),
                )
                .await?;
            if existing.is_empty() {
                let created = defaults
                    .create(
                        tx,
                        vec![
                            RawVals::new()
                                .set_cell("field_id", Cell::Id(fields.ids()[0]))
                                .set_cell("json_value", Cell::Str(target_id.to_string().into())),
                        ],
                    )
                    .await?;
                // Own the seed row so stock uninstall removes it, without claiming rows that
                // an administrator supplied before stock. The value itself stays editable.
                let name = format!("default_{field}");
                this.env()
                    .records("ir.model.data")?
                    .create(
                        tx,
                        vec![
                            RawVals::new()
                                .set_cell("module", Cell::Str("stock".into()))
                                .set_cell("name", Cell::Str(name.into()))
                                .set_cell("model", Cell::Str("ir.default".into()))
                                .set_cell("res_id", Cell::Id(created.ids()[0]))
                                .set_cell("noupdate", Cell::Bool(true)),
                        ],
                    )
                    .await?;
            }
        }
        Ok(())
    }
}
