// SPDX-License-Identifier: MIT OR Apache-2.0
//! Re-run the preceding selection with today's permissions. Only the query is persisted.
use erp_fields::{Error, Result};
use erp_sdk::ai::{LabelledData, ToolCall, ToolCallId, ToolName, ToolResult, ToolSpec};
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;

pub(super) fn spec() -> Result<ToolSpec> {
    let Json::Object(schema) =
        json!({"type":"object","properties":{},"additionalProperties":false})
    else {
        return Err(Error::programming("Invalid repeat_previous_read schema."));
    };
    ToolSpec::new(ToolName::new("repeat_previous_read").map_err(|e| Error::programming(e.to_string()))?,
        "Refresh the last result in this conversation with its original filters, sorting, grouping and limit. Use this first for follow-ups about the same records.",schema)
        .map_err(|e| Error::programming(e.to_string()))
}

pub(super) fn repeat(call: &ToolCall, previous: Option<&Json>) -> Result<ToolCall> {
    if call.arguments() != &json!({}) {
        return Err(Error::validation(
            "repeat_previous_read takes no arguments.",
        ));
    }
    let query = previous.ok_or_else(|| Error::validation("No saved query for the preceding turn. Reconstruct the selection from the previous question first."))?;
    saved(call.id(), query)
}

pub(super) fn saved(id: &ToolCallId, query: &Json) -> Result<ToolCall> {
    let name = query["tool"]
        .as_str()
        .filter(|s| matches!(*s, "aggregate_records" | "search_records"))
        .ok_or_else(|| Error::invariant("Invalid saved assistant query."))?;
    ToolCall::new(id.clone(), name.to_owned(), query["arguments"].clone())
        .map_err(|e| Error::programming(e.to_string()))
}

pub(super) fn wants_previous_prices(question: &str) -> bool {
    let normalized = question.to_lowercase();
    let words: Vec<_> = normalized
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .collect();
    // Only a plain catalog-price follow-up can bypass the general assistant. Additional fields,
    // currency conversions and mutations need to go through ordinary planning and authorization.
    if !words.iter().all(|word| {
        matches!(
            *word,
            "tolong"
                | "coba"
                | "tampilkan"
                | "tunjukkan"
                | "tambahkan"
                | "sertakan"
                | "berapa"
                | "harga"
                | "harganya"
                | "katalog"
                | "jual"
                | "produk"
                | "tersebut"
                | "tadi"
                | "juga"
                | "please"
                | "what"
                | "are"
                | "their"
                | "those"
                | "the"
                | "products"
                | "catalog"
                | "catalogue"
                | "price"
                | "prices"
                | "show"
                | "me"
                | "include"
                | "also"
                | "too"
        )
    }) {
        return false;
    }
    words
        .iter()
        .any(|s| matches!(*s, "harganya" | "harga" | "price" | "prices"))
        && words.iter().any(|s| {
            matches!(
                *s,
                "harganya" | "juga" | "tadi" | "tersebut" | "their" | "those" | "also" | "too"
            )
        })
}

// Filters must identify variants returned by the current authorized ranking. Template and variant IDs are independent.
pub(super) fn is_product_ranking(data: &Json) -> bool {
    matches!(
        data["model"].as_str(),
        Some("sale.order.line" | "purchase.order.line")
    ) && data.get("groupby").or_else(|| data.get("group_by")) == Some(&json!(["product_id"]))
}
/// Older sales rankings could have added quantities across different packaging units.
pub(super) fn needs_unit_rerun(args: &Json) -> bool {
    if args["model"] != "sale.order.line" || !is_product_ranking(args) {
        return false;
    }
    let sums_units = args["aggregates"].as_array().is_some_and(|measures| {
        measures.iter().any(|m| match m.as_str() {
            Some(spec) => matches!(spec, "product_uom_qty" | "product_uom_qty:sum"),
            None => m["field"] == "product_uom_qty" && m.get("function").is_none_or(|f| f == "sum"),
        })
    });
    let pinned_unit = args["domain"].as_array().is_some_and(|domain| {
        domain.iter().any(|term| term.as_array().is_some_and(|triple| matches!(
            triple.as_slice(), [field, op, id] if field == "product_uom_id" && op == "=" && id.as_i64().is_some_and(|v| v > 0)
        )))
    });
    sums_units && !pinned_unit
}

fn variant_id(group: &Json) -> Option<i64> {
    let value = &group["product_id"];
    value
        .as_i64()
        .or_else(|| value.get(0)?.as_i64())
        .or_else(|| value.get("id")?.as_i64())
        .filter(|n| *n > 0)
}
/// Build the price lookup from the live grouped result, never from model-supplied IDs.
pub(super) fn price_query(call: &ToolCall, ranking: &Json) -> Result<ToolCall> {
    let groups = ranking["groups"]
        .as_array()
        .ok_or_else(|| Error::validation("The product ranking is unavailable."))?;
    let ids: Vec<_> = groups
        .iter()
        .map(|g| {
            variant_id(g)
                .ok_or_else(|| Error::validation("The ranking has an unidentified product."))
        })
        .collect::<Result<_>>()?;
    if ids.is_empty() {
        return Err(Error::user("No products matched the preceding ranking."));
    }
    ToolCall::new(call.id().clone(),"search_records".into(),json!({"model":"product.product","domain":[["id","in",ids]],"fields":["lst_price"],"order":"id asc","limit":ids.len()}))
        .map_err(|e|Error::programming(e.to_string()))
}

/// Read each variant's delegated catalog currency, which may belong to an ancestor company.
pub(super) async fn catalog_currencies(
    tx: &mut erp_orm::Transaction,
    ids: &[i64],
) -> Result<BTreeMap<i64, String>> {
    use erp_sdk::prelude::Cell;
    let variants = tx
        .env()
        .records("product.product")?
        .browse(ids.iter().copied())?;
    let mut codes = BTreeMap::new();
    let mut by_variant = BTreeMap::new();
    for (id, currency) in variants
        .ids()
        .into_iter()
        .zip(variants.cells(tx, "currency_id").await?)
    {
        let Cell::Id(currency_id) = currency else {
            return Err(Error::validation("The product has no catalog currency."));
        };
        if let std::collections::btree_map::Entry::Vacant(entry) = codes.entry(currency_id) {
            let record = tx.env().records("res.currency")?.browse([currency_id])?;
            let Cell::Str(name) = record.cell(tx, "name").await? else {
                return Err(Error::validation("The catalog currency has no code."));
            };
            entry.insert(name.to_string());
        }
        by_variant.insert(id, codes[&currency_id].clone());
    }
    Ok(by_variant)
}

pub(super) fn remember(current: &mut Option<Json>, call: &ToolCall) {
    // Supporting record lists and an overall total must not replace a more specific grouping.
    let priority = |tool: &str, args: &Json| match tool {
        "aggregate_records" if is_product_ranking(args) => 3,
        "aggregate_records"
            if args
                .get("groupby")
                .or_else(|| args.get("group_by"))
                .and_then(Json::as_array)
                .is_some_and(|g| !g.is_empty()) =>
        {
            2
        }
        "aggregate_records" => 1,
        _ => 0,
    };
    if current.as_ref().is_none_or(|q| {
        priority(call.name(), call.arguments())
            >= priority(q["tool"].as_str().unwrap_or(""), &q["arguments"])
    }) {
        *current = Some(json!({"tool":call.name(),"arguments":call.arguments()}));
    }
}

pub(super) fn ranked_prices(
    ranking: &Json,
    prices: &ToolResult,
    currencies: &BTreeMap<i64, String>,
) -> Result<ToolResult> {
    let missing = || {
        Error::access("Catalog prices could not be verified for the complete product selection.")
    };
    let groups = ranking["groups"].as_array().ok_or_else(missing)?;
    let price_data = prices.data().value();
    let rows = price_data["records"].as_array().ok_or_else(missing)?;
    if price_data["truncated"] != false || rows.len() != groups.len() {
        return Err(missing());
    }
    let mut ordered = Vec::with_capacity(groups.len());
    for (index, group) in groups.iter().enumerate() {
        let id = variant_id(group).ok_or_else(missing)?;
        let value = rows
            .iter()
            .find(|r| r["id"].as_i64() == Some(id))
            .and_then(|r| r.get("lst_price"))
            .filter(|v| v.is_number() || v.is_string())
            .ok_or_else(missing)?;
        let variant = &group["product_id"];
        let label = variant
            .get(1)
            .or_else(|| variant.get("display_name"))
            .and_then(Json::as_str);
        let currency = currencies.get(&id).ok_or_else(missing)?;
        let mut row = json!({"rank":index+1, "id":id, "product":label, "catalog_price":value, "currency":currency});
        for measure in ranking["aggregates"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Json::as_str)
        {
            row[measure] = group.get(measure).ok_or_else(missing)?.clone();
        }
        ordered.push(row);
    }
    Ok(ToolResult::new(
        prices.id().clone(),
        LabelledData::new(
            prices.data().source().clone(),
            json!({"records":ordered,"truncated":false,"price_basis":"Catalog sales price in the product's company currency."}),
        ),
        false,
    ))
}

pub(super) fn resolve(
    call: &ToolCall,
    previous: Option<&Json>,
    product_prices: bool,
) -> Result<ToolCall> {
    if call.name() == "repeat_previous_read" {
        return repeat(call, previous);
    }
    if product_prices && matches!(call.name(), "search_records" | "aggregate_records") {
        if previous.is_some() {
            return Err(Error::validation(
                "Use repeat_previous_read. It refreshes the ranked products and includes their catalog prices.",
            ));
        }
        return Err(Error::validation(
            "Please rerun the previous ranking. This conversation has no saved query to verify its original selection.",
        ));
    }
    Ok(call.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use erp_sdk::ai::{DataSource, ToolCallId};
    fn call(name: &str, args: Json) -> ToolCall {
        ToolCall::new(ToolCallId::new("followup").unwrap(), name.into(), args).unwrap()
    }
    #[test]
    fn historical_sales_unit_rankings_must_have_a_pinned_unit_before_price_replay() {
        let mut args = json!({"model":"sale.order.line","group_by":["product_id"],"domain":[],"aggregates":[]});
        for measure in [
            json!("product_uom_qty:sum"),
            json!("product_uom_qty"),
            json!({"field":"product_uom_qty"}),
            json!({"field":"product_uom_qty","function":"sum"}),
        ] {
            args["aggregates"] = json!([measure]);
            assert!(needs_unit_rerun(&args));
        }
        args["domain"] = json!([["product_uom_id", "!=", 10]]);
        assert!(needs_unit_rerun(&args));
        args["domain"] = json!([["product_uom_id", "=", 10]]);
        assert!(!needs_unit_rerun(&args));
        args["domain"] = json!([]);
        args["model"] = json!("purchase.order.line");
        assert!(!needs_unit_rerun(&args));
        args["model"] = json!("sale.order.line");
        args["aggregates"] = json!(["price_subtotal:sum"]);
        assert!(!needs_unit_rerun(&args));
    }

    #[test]
    fn price_followup_refreshes_top_three_and_joins_by_variant_id_in_rank_order() {
        let args = json!({"model":"sale.order.line","domain":[["state","=","sale"]],"order":"product_uom_qty desc","groupby":["product_id"],"aggregates":["product_uom_qty:sum"],"limit":3});
        let mut receipt = None;
        remember(&mut receipt, &call("aggregate_records", args.clone()));
        remember(
            &mut receipt,
            &call(
                "aggregate_records",
                json!({"model":"sale.order","groupby":[]}),
            ),
        );
        remember(
            &mut receipt,
            &call(
                "search_records",
                json!({"model":"product.template","limit":20}),
            ),
        );
        let saved = receipt.unwrap();
        assert!(is_product_ranking(
            &json!({"model":"sale.order.line","group_by":["product_id"]})
        ));
        let replay = call("repeat_previous_read", json!({}));
        assert_eq!(
            resolve(&replay, Some(&saved), true).unwrap().arguments(),
            &args
        );
        assert!(resolve(&replay, None, true).is_err());
        assert!(resolve(&replay, Some(&json!({"tool":"run_action"})), false).is_err());
        let broad = call(
            "search_records",
            json!({"model":"product.template","domain":[["id",">",0]],"limit":20}),
        );
        assert!(resolve(&broad, Some(&saved), true).is_err());
        assert!(resolve(&broad, None, true).is_err());
        let other_ranking = call(
            "aggregate_records",
            json!({"model":"purchase.order.line","groupby":["product_id"],"limit":20}),
        );
        assert!(resolve(&other_ranking, None, true).is_err());
        assert!(wants_previous_prices("tampilkan harganya juga"));
        assert!(wants_previous_prices("Berapa harga katalog produk tadi?"));
        assert!(wants_previous_prices("What are their prices?"));
        assert!(!wants_previous_prices(
            "Buat quotation dan tambahkan harganya juga"
        ));
        assert!(!wants_previous_prices("Update their prices"));
        assert!(!wants_previous_prices("also show their prices and stock"));
        assert!(!wants_previous_prices("Tampilkan harganya juga dalam USD"));
        assert!(!wants_previous_prices("buat daftar harga semua produk"));
        assert!(!wants_previous_prices("tampilkan harga beli juga"));
        let groups = json!({"model":"sale.order.line","groupby":["product_id"],"aggregates":["product_uom_qty:sum"],"groups":[{"product_id":[83,"Desk"],"product_uom_qty:sum":12},{"product_id":[5,"Chair"],"product_uom_qty:sum":6},{"product_id":[120,"Pen"],"product_uom_qty:sum":4}]});
        let lookup = price_query(&replay, &groups).unwrap();
        assert_eq!(
            lookup.arguments()["domain"],
            json!([["id", "in", [83, 5, 120]]])
        );
        let data = |rows: Json| {
            ToolResult::new(
                replay.id().clone(),
                LabelledData::new(
                    DataSource::new(
                        ToolName::new("search_records").unwrap(),
                        Some("product.product".into()),
                        vec![5, 83, 120],
                    )
                    .unwrap(),
                    json!({"records":rows,"truncated":false}),
                ),
                false,
            )
        };
        let prices = data(
            json!([{"id":5,"lst_price":15},{"id":83,"lst_price":180},{"id":120,"lst_price":0.8}]),
        );
        let currencies =
            BTreeMap::from([(83, "IDR".into()), (5, "USD".into()), (120, "IDR".into())]);
        let records = ranked_prices(&groups, &prices, &currencies).unwrap();
        assert_eq!(
            records.data().value()["records"],
            json!([
                {"rank":1,"id":83,"product":"Desk","catalog_price":180,"currency":"IDR","product_uom_qty:sum":12},
                {"rank":2,"id":5,"product":"Chair","catalog_price":15,"currency":"USD","product_uom_qty:sum":6},
                {"rank":3,"id":120,"product":"Pen","catalog_price":0.8,"currency":"IDR","product_uom_qty:sum":4}
            ])
        );
        assert!(
            ranked_prices(
                &groups,
                &data(json!([{"id":83,"lst_price":180}])),
                &currencies
            )
            .is_err()
        );
    }
}
