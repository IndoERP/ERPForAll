// SPDX-License-Identifier: MIT OR Apache-2.0
//! Draft rule helpers; write_base updates the onchange cache, not stored rows.
use crate::route::StockRule;
use erp_sdk::prelude::*;

#[methods(StockRule, addon = "stock")]
impl StockRule {
    #[onchange("picking_type_id")]
    async fn _onchange_rule_type(
        this: &RecordSet<StockRule>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        for rule in this.as_records().iter() {
            let operation = rule.related(tx, "picking_type_id").await?;
            if !operation.is_empty() {
                let source = operation.cell(tx, "default_location_src_id").await?;
                let destination = operation.cell(tx, "default_location_dest_id").await?;
                rule.write_base(
                    tx,
                    RawVals::new()
                        .set_cell("location_src_id", source)
                        .set_cell("location_dest_id", destination),
                )
                .await?;
            }
        }
        Ok(OnchangeResult::none())
    }

    #[onchange("route_id", "company_id")]
    async fn _onchange_rule_route(
        this: &RecordSet<StockRule>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        for rule in this.as_records().iter() {
            let route = rule.related(tx, "route_id").await?;
            let company = route.cell(tx, "company_id").await?;
            let mut vals = RawVals::new();
            if !matches!(company, Cell::Null) {
                vals = vals.set_cell("company_id", company.clone());
            }
            // A shared route included: its empty company differs from any warehouse's company.
            let operation = rule.related(tx, "picking_type_id").await?;
            let warehouse = operation.related(tx, "warehouse_id").await?;
            let warehouse_company = if warehouse.is_empty() {
                Cell::Null
            } else {
                warehouse.cell(tx, "company_id").await?
            };
            if !operation.is_empty() && warehouse_company != company {
                vals = vals.set_cell("picking_type_id", Cell::Null);
            }
            if !vals.is_empty() {
                rule.write_base(tx, vals).await?;
            }
        }
        Ok(OnchangeResult::none())
    }
}
