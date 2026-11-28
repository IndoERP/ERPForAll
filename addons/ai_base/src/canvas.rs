// SPDX-License-Identifier: MIT OR Apache-2.0
//! Assistant canvas rendering tool definition and payload handling.
use erp_sdk::{
    ai::{DataSource, LabelledData, ToolCall, ToolName, ToolResult, ToolSpec},
    prelude::*,
};
use serde_json::{Value as Json, json};

mod layout;

const TOOL: &str = "render_canvas";

fn name() -> Result<ToolName> {
    ToolName::new(TOOL).map_err(|e| Error::programming(e.to_string()))
}

/// A provider tool for commanding the AI Workspace dynamic canvas.
///
/// # Errors
/// Returns a programming error if the built-in tool schema is invalid.
pub fn render_canvas_spec() -> Result<ToolSpec> {
    let Json::Object(schema) = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "canvas_type": {
                "type": "string",
                "enum": ["chart", "table", "pivot", "dashboard", "flow", "simulation", "document", "brief", "timeline", "checklist", "slides"],
                "description": "Choose an analytical, planning, or editorial layout. Multiple calls create separate outputs."
            },
            "title": {
                "type": "string",
                "description": "Heading or title for the canvas card"
            },
            "description": {
                "type": "string",
                "description": "Brief takeaway or subtitle explaining the visualization"
            },
            "data_json": {
                "type": "string",
                "description": "JSON object. Analytics: {} after a matching ERP read. Document/brief: {sections:[{kind:'heading'|'paragraph'|'callout',text}|{kind:'list',items:[string]}|{kind:'timeline'|'checklist',items:[...]}]}. Optional document sections: {kind:'table',columns:[string],rows:[[string]]}, {kind:'facts',items:[{label,value}]}, {kind:'code',text,language?}. Timeline: {items:[{label,when,detail?}]}. Checklist: {items:[{label,state}]}. Flow: {steps:[{label,state,description?}]}. States pending/active/completed/cancelled. Slides: {slides:[{heading,points:[string]}]}. Simulation: {baseRevenue,baseMarginPct,variables:[{key:'volume_growth'|'discount',label,min,max,initial,step?}]}. Up to 32 items per list, 40 slides."
            }
        },
        "required": ["canvas_type", "title", "description", "data_json"]
    }) else {
        return Err(Error::programming("Invalid render_canvas schema."));
    };

    ToolSpec::new(
        name()?,
        "Render an interactive visual widget, chart, table, or animated workflow diagram on the AI Workspace canvas.",
        schema,
    )
    .map_err(|e| Error::programming(e.to_string()))
}

/// Validate and stage a `render_canvas` tool invocation.
///
/// # Errors
/// Returns a validation error if arguments are missing or if `data_json` is not valid JSON.
pub fn render_canvas(call: &ToolCall) -> Result<ToolResult> {
    let payload = extract_canvas_payload(call)?;
    let canvas_type = &payload["canvas_type"];
    let title = &payload["title"];
    let source = DataSource::new(name()?, None, Vec::new())
        .map_err(|e| Error::programming(e.to_string()))?;

    Ok(ToolResult::new(
        call.id().clone(),
        LabelledData::new(
            source,
            json!({
                "status": "staged",
                "canvas_type": canvas_type,
                "title": title
            }),
        ),
        false,
    ))
}

/// Extract the full parsed canvas object ready for sending to the web client.
pub fn extract_canvas_payload(call: &ToolCall) -> Result<Json> {
    let args = call
        .arguments()
        .as_object()
        .ok_or_else(|| Error::validation("render_canvas arguments must be an object."))?;

    let canvas_type = args
        .get("canvas_type")
        .and_then(Json::as_str)
        .ok_or_else(|| Error::validation("render_canvas requires canvas_type."))?;

    let title = args
        .get("title")
        .and_then(Json::as_str)
        .ok_or_else(|| Error::validation("render_canvas requires title."))?;

    let description = args
        .get("description")
        .and_then(Json::as_str)
        .ok_or_else(|| Error::validation("Canvas description must be text."))?;
    if title.trim().is_empty() || title.len() > 256 || description.len() > 1024 {
        return Err(Error::validation(
            "Enter a canvas title up to 256 bytes and a description up to 1024 bytes.",
        ));
    }

    let data_json = args
        .get("data_json")
        .and_then(Json::as_str)
        .ok_or_else(|| Error::validation("render_canvas requires data_json."))?;

    if data_json.len() > 65536 {
        return Err(Error::validation(
            "render_canvas data_json exceeds 64KB limit.",
        ));
    }

    let parsed_data: Json = serde_json::from_str(data_json).map_err(|err| {
        Error::validation(format!("render_canvas data_json is not valid JSON: {err}"))
    })?;

    layout::authored(canvas_type, &parsed_data)?;
    Ok(json!({
        "canvas_type": canvas_type,
        "title": title,
        "description": description,
        "data": parsed_data
    }))
}

/// Builds analytical cards from data returned by an authorized ERP query in this turn.
/// The model controls the requested layout; numbers, categories and provenance come from ERP.
///
/// # Errors
/// Refuses a quantitative card without an appropriate successful query.
pub fn grounded_canvas_payload(call: &ToolCall, data: Option<&LabelledData>) -> Result<Json> {
    let payload = extract_canvas_payload(call)?;
    let Some(kind @ ("chart" | "table" | "dashboard" | "pivot")) = payload["canvas_type"].as_str()
    else {
        return Ok(payload);
    };
    let source = data.ok_or_else(|| {
        Error::validation("Run a matching ERP query before rendering an analytical card.")
    })?;
    let title = payload["title"]
        .as_str()
        .ok_or_else(|| Error::invariant("Validated canvas has no title."))?;
    analytical_output(kind, title, source)
}

/// Build the same sourced analytical card for an explicit tool call or an automatic visualization.
///
/// # Errors
/// Returns a validation error when the source does not support the requested analytical layout.
pub fn analytical_output(kind: &str, title: &str, source: &LabelledData) -> Result<Json> {
    if !matches!(kind, "chart" | "table" | "dashboard" | "pivot")
        || title.trim().is_empty()
        || title.len() > 256
    {
        return Err(Error::validation("Invalid analytical card type or title."));
    }
    let mut payload = json!({"canvas_type":kind, "title":title});
    let unavailable =
        || Error::validation("Run a matching ERP query before rendering an analytical card.");
    let model = source.source().model().ok_or_else(unavailable)?;
    let result = source.value();
    let mut card = match source.source().tool().as_str() {
        "search_records" if kind == "table" => {
            let rows = result["records"].as_array().ok_or_else(unavailable)?;
            json!({"records":rows,"truncated":result["truncated"],"source_domain":[["id","in",source.source().ids()]]})
        }
        "aggregate_records" => {
            let rows = result["groups"].as_array().ok_or_else(unavailable)?;
            let dimensions: Vec<_> = result["groupby"]
                .as_array()
                .ok_or_else(unavailable)?
                .iter()
                .map(|g| g.as_str().ok_or_else(unavailable))
                .collect::<Result<_>>()?;
            let measures: Vec<_> = result["aggregates"]
                .as_array()
                .ok_or_else(unavailable)?
                .iter()
                .map(|spec| spec.as_str().ok_or_else(unavailable))
                .collect::<Result<_>>()?;
            let first = *measures.first().ok_or_else(unavailable)?;
            let mut series = Vec::new();
            for row in rows {
                let mut labels = Vec::with_capacity(dimensions.len());
                for field in &dimensions {
                    let category = row.get(*field).ok_or_else(unavailable)?;
                    labels.push(match category {
                        Json::String(s) => s.clone(),
                        Json::Array(pair) if pair.len() == 2 => {
                            pair[1].as_str().ok_or_else(unavailable)?.to_owned()
                        }
                        Json::Bool(false) | Json::Null => "—".to_owned(),
                        other => other.to_string(),
                    });
                }
                let label = if labels.is_empty() {
                    "All matching records".into()
                } else {
                    labels.join(" · ")
                };
                let value = row.get(first).ok_or_else(unavailable)?;
                if value.is_number() {
                    series.push(json!({"label":label,"value":value}));
                } else if let Some(n) = value
                    .as_str()
                    .and_then(|text| text.parse::<f64>().ok())
                    .filter(|n| n.is_finite())
                {
                    // Decimal money is represented as text on the wire. Keep the exact string in the table.
                    series.push(json!({"label":label,"value":n}));
                }
            }
            if !rows.is_empty() && series.is_empty() && matches!(kind, "chart" | "dashboard") {
                return Err(Error::validation(
                    "This measure has no numeric values to chart. Choose a table.",
                ));
            }
            let measure_specs: Vec<_> = measures
                .iter()
                .map(|s| json!({"key":s,"label":s}))
                .collect();
            let columns: Vec<_> = dimensions.iter().chain(&measures).collect();
            json!({"records":rows,"columns":columns,"dimensions":dimensions,"measures":measure_specs,
                "series":series,"total_groups":result["total_groups"],"truncated":result["truncated"]})
        }
        _ => return Err(unavailable()),
    };
    if source.source().tool().as_str() == "aggregate_records"
        && let Some(mut query) = result["query"].as_object().cloned()
    {
        card["source_domain"] = query.get("domain").cloned().ok_or_else(unavailable)?;
        query.insert("title".into(), payload["title"].clone());
        query.insert(
            "visualization".into(),
            json!(if matches!(kind, "chart" | "dashboard") {
                "bar"
            } else {
                "table"
            }),
        );
        card["analysis_definition"] = json!({"panels":[query]});
    }
    card["source_model"] = json!(model);
    payload["description"] = json!(format!("ERP · {model}"));
    payload["data"] = card;
    Ok(payload)
}

#[cfg(test)]
mod grounding_tests {
    use super::*;
    use erp_sdk::ai::ToolCallId;

    fn requested(kind: &str) -> ToolCall {
        ToolCall::new(ToolCallId::new("canvas1").unwrap(), "render_canvas".into(),
            json!({"canvas_type":kind,"title":"Sales","description":"bogus 999","data_json":"{\"series\":[{\"label\":\"Fabricated\",\"value\":999}]}"})
        ).unwrap()
    }

    #[test]
    fn composed_planning_canvases_validate_sections_and_cannot_claim_live_sources() {
        let make = |kind: &str, data: Json| {
            ToolCall::new(
            ToolCallId::new("canvas-section").unwrap(), "render_canvas".into(),
            json!({"canvas_type":kind,"title":"Go live","description":"","data_json":data.to_string()}),
        ).unwrap()
        };
        let content = json!({"sections":[{"kind":"heading","text":"Purchasing"},{"kind":"checklist","items":[{"label":"Confirm taxes","state":"pending"}]}]});
        let call = make("brief", content.clone());
        assert!(render_canvas(&call).is_ok());
        assert_eq!(
            grounded_canvas_payload(&call, None).unwrap()["data"],
            content
        );
        for invalid in [
            json!({"sections":[]}),
            json!({"sections":[{"kind":"table","columns":["A","B"],"rows":[["missing cell"]]}]}),
            json!({"sections":[{"kind":"code","text":"sample","url":"external"}]}),
            json!({"sections":[{"kind":"html","text":"demo"}]}),
            json!({"sections":[{"kind":"list","items": ["ok"]}], "source_model":"sale.order"}),
            json!({"sections":[{"kind":"paragraph","text":"x".repeat(2049)}]}),
            json!({"sections": vec![json!({"kind":"paragraph","text":"x"});33]}),
        ] {
            let call = make("document", invalid);
            assert!(grounded_canvas_payload(&call, None).is_err());
        }
        for (kind, data) in [
            (
                "slides",
                json!({"slides":[{"heading":"Cutover","points":["Verify opening balances"]}]}),
            ),
            (
                "timeline",
                json!({"items":[{"label":"Pilot","when":"Week one"}]}),
            ),
            (
                "checklist",
                json!({"items":[{"label":"Verify roles","state":"active"}]}),
            ),
        ] {
            let call = make(kind, data);
            assert!(render_canvas(&call).is_ok());
            assert!(grounded_canvas_payload(&call, None).is_ok());
        }
    }

    #[test]
    fn analytical_cards_use_tool_rows_and_refuse_unsourced_figures() {
        let call = requested("chart");
        assert!(grounded_canvas_payload(&call, None).is_err());
        let source = DataSource::new(
            ToolName::new("aggregate_records").unwrap(),
            Some("sale.order".into()),
            vec![],
        )
        .unwrap();
        let actual = LabelledData::new(
            source,
            json!({"groupby":["state"],"aggregates":["id:count"],"groups":[{"state":"sale","id:count":3}]}),
        );
        let result = grounded_canvas_payload(&call, Some(&actual)).unwrap();
        assert_eq!(
            result["data"]["series"],
            json!([{"label":"sale","value":3}])
        );
        assert_eq!(result["data"]["records"][0]["id:count"], 3);
        assert!(!result.to_string().contains("999"));
    }
}
