// SPDX-License-Identifier: MIT OR Apache-2.0
use super::id;
use erp_sdk::{
    Operation,
    prelude::*,
    serde_json::{Map, Value as Json},
};
use std::collections::BTreeSet;

pub(super) async fn tax_map(position: &Records, tx: &mut Transaction) -> Result<Json> {
    let mut map = Map::new();
    for tax in position.related(tx, "tax_ids").await?.iter() {
        if tax.cell(tx, "active").await? != Cell::Bool(true) {
            continue;
        }
        for original in tax.related(tx, "original_tax_ids").await?.ids() {
            map.entry(original.to_string())
                .or_insert_with(|| Json::Array(Vec::new()))
                .as_array_mut()
                .ok_or_else(|| Error::internal("Invalid internal tax map."))?
                .push(Json::from(tax.ids()[0]));
        }
    }
    Ok(Json::Object(map))
}
pub(super) async fn account_map(position: &Records, tx: &mut Transaction) -> Result<Json> {
    let mut map = Map::new();
    for row in position.related(tx, "account_ids").await?.iter() {
        let src = id(row.cell(tx, "account_src_id").await?)?
            .ok_or_else(|| Error::internal("Account mapping source is missing."))?;
        let dest = id(row.cell(tx, "account_dest_id").await?)?
            .ok_or_else(|| Error::internal("Account mapping destination is missing."))?;
        map.insert(src.to_string(), Json::from(dest));
    }
    Ok(Json::Object(map))
}
pub(super) async fn taxes(
    position: &Records,
    tx: &mut Transaction,
    input: Vec<i64>,
) -> Result<Vec<i64>> {
    let model = position.env().records("account.tax")?;
    model
        .browse(input.clone())?
        .check_access(tx, Operation::Read)
        .await?;
    if position.is_empty() {
        return Ok(input);
    }
    position
        .ensure_one()?
        .check_access(tx, Operation::Read)
        .await?;
    let mappings = tax_map(position, tx).await?;
    let mut seen = BTreeSet::new();
    let mut output = Vec::new();
    for tax in model.browse(input)?.iter() {
        let replacements = if let Some(mapped) = mappings.get(tax.ids()[0].to_string()) {
            mapped
                .as_array()
                .ok_or_else(|| Error::internal("Invalid tax replacement list."))?
                .iter()
                .map(|v| {
                    v.as_i64()
                        .ok_or_else(|| Error::internal("Invalid tax replacement id."))
                })
                .collect::<Result<Vec<_>>>()?
        } else {
            let positions = tax.related(tx, "fiscal_position_ids").await?;
            if positions.is_empty() || positions.ids().contains(&position.ids()[0]) {
                tax.ids()
            } else {
                Vec::new()
            }
        };
        for dest in replacements {
            if seen.insert(dest) {
                output.push(dest);
            }
        }
    }
    model
        .browse(output.clone())?
        .check_access(tx, Operation::Read)
        .await?;
    Ok(output)
}
pub(super) async fn account(position: &Records, tx: &mut Transaction, account: i64) -> Result<i64> {
    let model = position.env().records("account.account")?;
    model
        .browse([account])?
        .check_access(tx, Operation::Read)
        .await?;
    if position.is_empty() {
        return Ok(account);
    }
    position
        .ensure_one()?
        .check_access(tx, Operation::Read)
        .await?;
    let map = account_map(position, tx).await?;
    let mapped = match map.get(account.to_string()) {
        Some(value) => value
            .as_i64()
            .ok_or_else(|| Error::internal("Invalid account replacement id."))?,
        None => account,
    };
    model
        .browse([mapped])?
        .check_access(tx, Operation::Read)
        .await?;
    Ok(mapped)
}
