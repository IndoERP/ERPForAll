// SPDX-License-Identifier: MIT OR Apache-2.0
//! Scoped packaging-barcode window actions; context never grants record access.
use crate::packaging::product_context;
use crate::{ProductProduct, ProductTemplate, UomUomPackagings};
use erp_addon_uom::Uom;
use erp_sdk::prelude::*;
use erp_sdk::{Operation, ValueMap};

async fn existing(records: &Records, tx: &mut Transaction) -> Result<i64> {
    records.check_access(tx, Operation::Read).await?;
    let id = records.ensure_one()?.int()?;
    if records.exists(tx).await?.is_empty() {
        return Err(Error::missing(
            "The packaging action needs an existing record.",
        ));
    }
    Ok(id)
}
fn term(field: &str, op: &str, value: Value) -> Value {
    Value::list([Value::str(field), Value::str(op), value])
}
async fn action(
    env: &Env,
    tx: &mut Transaction,
    domain: Vec<Value>,
    context: ValueMap,
) -> Result<Value> {
    let metadata = env.sudo(true);
    let data = metadata
        .records("ir.model.data")?
        .search(
            tx,
            &(Domain::condition("module", Operator::Eq, "product")?
                & Domain::condition("name", Operator::Eq, "product_uom_action_view_list")?),
        )
        .await?;
    let action_id = match data.cell(tx, "res_id").await? {
        Cell::Id(id) => id,
        _ => {
            return Err(Error::programming(
                "The packaging action's external identity is missing or invalid.",
            ));
        }
    };
    let action_record = metadata
        .records("ir.actions.act_window")?
        .browse([action_id])?;
    if action_record.value(tx, "res_model").await? != Value::str("product.uom") {
        return Err(Error::validation(
            "The packaging action must target product.uom.",
        ));
    }
    let title = action_record.value(tx, "name").await?;
    let target = action_record.value(tx, "target").await?;
    let views = action_record.value(tx, "views").await?;
    Ok(Value::Map(Arc::new(ValueMap::from([
        ("type".into(), Value::str("ir.actions.act_window")),
        ("name".into(), title),
        ("res_model".into(), Value::str("product.uom")),
        ("target".into(), target),
        ("views".into(), views),
        ("domain".into(), Value::list(domain)),
        ("context".into(), Value::Map(Arc::new(context))),
    ]))))
}
#[methods(ProductTemplate, addon = "product")]
impl ProductTemplate {
    /// Open packaging barcodes for the visible variants of one readable template.
    #[method]
    #[rpc(effect = read)]
    async fn action_open_packaging_barcodes(
        this: &RecordSet<ProductTemplate>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let id = existing(this.as_records(), tx).await?;
        let products = this
            .env()
            .records("product.product")?
            .search(tx, &Domain::condition("product_tmpl_id", Operator::Eq, id)?)
            .await?;
        let ids = products.ids();
        let first = ids.first().ok_or_else(|| {
            Error::user("This product has no visible variant for packaging barcodes.")
        })?;
        action(
            this.env(),
            tx,
            vec![term(
                "product_id",
                "in",
                Value::list(ids.iter().copied().map(Value::Int)),
            )],
            ValueMap::from([
                ("default_product_id".into(), Value::Int(*first)),
                (
                    "product_ids".into(),
                    Value::list(ids.into_iter().map(Value::Int)),
                ),
            ]),
        )
        .await
    }
}
#[methods(ProductProduct, addon = "product")]
impl ProductProduct {
    /// Open packaging barcodes for one readable variant.
    #[method]
    #[rpc(effect = read)]
    async fn action_open_packaging_barcodes(
        this: &RecordSet<ProductProduct>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let id = existing(this.as_records(), tx).await?;
        action(
            this.env(),
            tx,
            vec![term("product_id", "=", Value::Int(id))],
            ValueMap::from([
                ("default_product_id".into(), Value::Int(id)),
                ("product_ids".into(), Value::list([Value::Int(id)])),
                ("create".into(), Value::Bool(true)),
            ]),
        )
        .await
    }
}
#[methods(Uom, addon = "product")]
impl UomUomPackagings {
    /// Open this unit's packaging barcodes, optionally intersected with product_ids.
    #[method]
    #[rpc(effect = read)]
    async fn action_open_packaging_barcodes(
        this: &RecordSet<Uom>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let id = existing(this.as_records(), tx).await?;
        let (_, products) = product_context(this.env())?;
        let mut domain = vec![term("uom_id", "=", Value::Int(id))];
        if !products.is_empty() {
            domain.push(term(
                "product_id",
                "in",
                Value::list(products.into_iter().map(Value::Int)),
            ));
        }
        let mut context: ValueMap = this
            .env()
            .context()
            .to_map_bounded(64)?
            .into_iter()
            .collect();
        context.insert("default_uom_id".into(), Value::Int(id));
        action(this.env(), tx, domain, context).await
    }
}
