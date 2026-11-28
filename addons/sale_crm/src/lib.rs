// SPDX-License-Identifier: MIT OR Apache-2.0
//! Connect CRM to Sales only when both applications are installed.
use erp_sdk::prelude::*;

erp_sdk::addon!(
    name = "sale_crm",
    depends = [
        "base",
        "crm",
        "mail",
        "reporting",
        "sale",
        "sales_team",
        "web"
    ]
);
erp_sdk::model_ref!(CrmLead = "crm.lead");

erp_sdk::model_ref!(pub SaleOrder = "sale.order" {
    state: Option<Arc<str>>, currency_id: Option<RecId>,
    currency_rate: Decimal, amount_untaxed: Decimal
});

fn open_record(name: &str, record: &Records) -> Result<Value> {
    Ok(Value::Map(Arc::new(erp_sdk::ValueMap::from([
        ("type".into(), Value::str("ir.actions.act_window")),
        ("name".into(), Value::str(name)),
        ("res_model".into(), Value::str(record.model_name())),
        ("target".into(), Value::str("current")),
        ("res_id".into(), Value::Int(one_id(record)?)),
        (
            "views".into(),
            Value::list([Value::list([Value::False, Value::str("form")])]),
        ),
    ]))))
}

mod booking;
mod opportunity;
mod order;

/// Publish a lead row version for every change to its related sales documents.
/// A repeatable-read transaction with an older sibling-order snapshot must retry,
/// including when the opportunity was already won and none of its own fields changed.
async fn fence_linked_opportunities(tx: &mut Transaction, leads: &Records) -> Result<()> {
    leads.check_access(tx, erp_sdk::Operation::Write).await?;
    leads.lock_for_update(tx, true).await?;
    tx.execute(
        "UPDATE crm_lead SET write_date = write_date WHERE id = ANY($1)",
        &[&leads.ids()],
    )
    .await?;
    Ok(())
}

fn one_id(records: &Records) -> Result<i64> {
    records
        .id()?
        .and_then(|id| id.as_real())
        .map(|id| id.get())
        .ok_or_else(|| Error::user("Save the record before continuing."))
}

async fn optional_id(tx: &mut Transaction, record: &Records, field: &str) -> Result<Option<i64>> {
    match record.cell(tx, field).await? {
        Cell::Id(id) => Ok(Some(id)),
        Cell::Null => Ok(None),
        _ => Err(Error::invariant(format!("Expected a relation in {field}."))),
    }
}

async fn required_id(tx: &mut Transaction, record: &Records, field: &str) -> Result<i64> {
    optional_id(tx, record, field)
        .await?
        .ok_or_else(|| Error::user(format!("Set {field} before creating a linked quotation.")))
}
