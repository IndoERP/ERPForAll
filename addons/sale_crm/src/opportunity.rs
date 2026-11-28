// SPDX-License-Identifier: MIT OR Apache-2.0
use erp_sdk::prelude::*;
use erp_sdk::{Operation, ValueMap};
use std::collections::BTreeMap;

use crate::{CrmLead, one_id, open_record, optional_id, required_id};

#[extend(CrmLead, addon = "sale_crm")]
pub struct OpportunityOrders {
    #[field(
        one2many,
        comodel = "sale.order",
        inverse_name = "opportunity_id",
        readonly,
        copy = false,
        string = "Sales Documents"
    )]
    pub order_ids: Vec<RecId>,
    #[field(
        integer,
        compute = "_compute_quotation_count",
        string = "Quotations and Orders"
    )]
    pub quotation_count: i32,
}

fn refuse_reverse_link(values: &RawVals, context: &erp_sdk::Context) -> Result<()> {
    if values.contains("order_ids")
        || values.commands("order_ids").is_some()
        || context.contains("default_order_ids")
    {
        return Err(Error::validation("Manage linked quotations in Sales."));
    }
    Ok(())
}

#[methods(CrmLead, addon = "sale_crm")]
impl OpportunityOrders {
    #[compute(depends = ["order_ids", "order_ids.user_id"], depends_context = ["uid", "allowed_company_ids"])]
    async fn _compute_quotation_count(
        this: &RecordSet<CrmLead>,
        tx: &mut Transaction,
        out: &mut Assign<'_, CrmLead>,
    ) -> Result<()> {
        let records = this.as_records();
        let mut counts = BTreeMap::new();
        let source = records.origin().ids();
        if !source.is_empty() {
            let args = ReadGroup::new(Domain::field("opportunity_id")?.in_(source))
                .groupby(["opportunity_id"])
                .aggregates(["__count"]);
            let groups = records
                .env()
                .records("sale.order")?
                ._read_group(tx, args)
                .await?;
            for row in 0..groups.len() {
                let Some(id) = groups.column(0)[row].record_id() else {
                    continue;
                };
                let GroupValue::Int(count) = groups.column(1)[row] else {
                    return Err(Error::invariant("Invalid sales document count."));
                };
                counts.insert(
                    id.get(),
                    i32::try_from(count).map_err(|_| {
                        Error::invariant("Sales document count exceeds the field size.")
                    })?,
                );
            }
        }
        for (index, id) in records.rec_ids().iter().enumerate() {
            let count = id
                .as_real()
                .or(id.origin())
                .and_then(|id| counts.get(&id.get()))
                .copied()
                .unwrap_or(0);
            out.set(index, Self::QUOTATION_COUNT, count)?;
        }
        Ok(())
    }

    #[method]
    #[rpc(effect = read)]
    async fn action_view_quotations(
        this: &RecordSet<CrmLead>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let records = this.as_records();
        records.check_access(tx, Operation::Read).await?;
        let id = one_id(records)?;
        Ok(Value::Map(Arc::new(ValueMap::from([
            ("type".into(), Value::str("ir.actions.act_window")),
            ("name".into(), Value::str("Quotations and Orders")),
            ("res_model".into(), Value::str("sale.order")),
            ("target".into(), Value::str("current")),
            (
                "views".into(),
                Value::list(
                    ["list", "form"].map(|kind| Value::list([Value::False, Value::str(kind)])),
                ),
            ),
            (
                "domain".into(),
                Value::list([Value::list([
                    Value::str("opportunity_id"),
                    Value::str("="),
                    Value::Int(id),
                ])]),
            ),
            (
                "context".into(),
                Value::Map(Arc::new(ValueMap::from([(
                    "create".into(),
                    Value::Bool(false),
                )]))),
            ),
        ]))))
    }

    #[method]
    #[rpc(effect = write)]
    async fn action_create_quotation(
        this: &RecordSet<CrmLead>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let lead = this.as_records();
        let id = one_id(lead)?;
        lead.check_access(tx, Operation::Write).await?;
        lead.lock_for_update(tx, true).await?;
        if lead.cell(tx, "kind").await? != Cell::Str("opportunity".into())
            || lead.cell(tx, "state").await? == Cell::Str("lost".into())
        {
            return Err(Error::user(
                "Create a quotation from an open or won opportunity.",
            ));
        }
        let company = required_id(tx, lead, "company_id").await?;
        let mut vals = RawVals::new()
            .set_cell("opportunity_id", Cell::Id(id))
            .set_cell("company_id", Cell::Id(company))
            .set_cell(
                "partner_id",
                Cell::Id(required_id(tx, lead, "partner_id").await?),
            )
            .set_cell("origin", lead.cell(tx, "name").await?);
        for field in ["user_id", "team_id"] {
            if let Some(value) = optional_id(tx, lead, field).await? {
                vals = vals.set_cell(field, Cell::Id(value));
            }
        }
        let env = this
            .env()
            .with_company(company)?
            .without_default_overrides();
        let order = env.records("sale.order")?.create(tx, vec![vals]).await?;
        open_record("Quotation", &order)
    }

    #[method(override)]
    async fn create(
        this: &RecordSet<CrmLead>,
        tx: &mut Transaction,
        rows: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        for row in &rows {
            refuse_reverse_link(row, this.env().context())?;
        }
        sup.call(tx, this.as_records(), (rows,)).await
    }

    /// The commercial party/company on a quotation cannot drift when CRM users edit a deal.
    #[method(override)]
    async fn write(
        this: &RecordSet<CrmLead>,
        tx: &mut Transaction,
        values: RawVals,
        sup: Super,
    ) -> Result<()> {
        let leads = this.as_records();
        refuse_reverse_link(&values, this.env().context())?;
        if values.contains("company_id") || values.contains("partner_id") {
            leads.check_access(tx, Operation::Write).await?;
            leads.lock_for_update(tx, true).await?;
            tx.flush().await?;
            for field in ["company_id", "partner_id"] {
                if let Some(cell) = values.get(field) {
                    let desired = match cell {
                        Cell::Id(id) => Some(*id),
                        Cell::Null => None,
                        _ => return Err(Error::validation(format!("Invalid {field}."))),
                    };
                    let query = format!(
                        "SELECT 1 FROM sale_order WHERE opportunity_id = ANY($1) AND {field} IS DISTINCT FROM $2::int8 LIMIT 1"
                    );
                    if !tx
                        .query(&query, &[&leads.ids(), &desired])
                        .await?
                        .is_empty()
                    {
                        return Err(Error::user(
                            "The opportunity must keep the customer and company of its linked sales documents.",
                        ));
                    }
                }
            }
        }
        sup.call(tx, leads, (values,)).await
    }
}
