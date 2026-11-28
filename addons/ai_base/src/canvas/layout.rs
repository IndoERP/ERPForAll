// SPDX-License-Identifier: MIT OR Apache-2.0
//! The provider describes content and layout. The client supplies all controls and behavior.
use erp_sdk::prelude::{Error, Result};
use serde_json::Value as Json;

fn shape(value: &Json, fields: &[&str]) -> bool {
    value
        .as_object()
        .is_some_and(|obj| obj.keys().all(|key| fields.contains(&key.as_str())))
}

fn text(value: &Json, max: usize) -> bool {
    value
        .as_str()
        .is_some_and(|s| !s.trim().is_empty() && s.chars().count() <= max)
}

fn items(value: &Json, max: usize, check: impl Fn(&Json) -> bool) -> bool {
    value
        .as_array()
        .is_some_and(|rows| !rows.is_empty() && rows.len() <= max && rows.iter().all(check))
}

fn step_state(value: &Json) -> bool {
    matches!(
        value.as_str(),
        Some("pending" | "active" | "completed" | "cancelled")
    )
}

fn checklist(item: &Json) -> bool {
    shape(item, &["label", "state"]) && text(&item["label"], 2048) && step_state(&item["state"])
}

fn milestone(item: &Json) -> bool {
    shape(item, &["label", "when", "detail"])
        && text(&item["label"], 2048)
        && text(&item["when"], 2048)
        && item.get("detail").is_none_or(|detail| text(detail, 2048))
}

fn comparison(part: &Json) -> bool {
    if !shape(part, &["kind", "columns", "rows"])
        || !items(&part["columns"], 12, |column| text(column, 2048))
    {
        return false;
    }
    let columns = part["columns"].as_array().map_or(0, Vec::len);
    items(&part["rows"], 32, |row| {
        row.as_array().is_some_and(|cells| {
            cells.len() == columns
                && cells
                    .iter()
                    .all(|v| v.as_str().is_some_and(|s| s.chars().count() <= 2048))
        })
    })
}

fn section(part: &Json) -> bool {
    match part["kind"].as_str() {
        Some("heading" | "paragraph" | "callout") => {
            shape(part, &["kind", "text"]) && text(&part["text"], 2048)
        }
        Some("table") => comparison(part),
        Some("facts") => {
            shape(part, &["kind", "items"])
                && items(&part["items"], 32, |v| {
                    shape(v, &["label", "value"])
                        && text(&v["label"], 2048)
                        && text(&v["value"], 2048)
                })
        }
        Some("code") => {
            shape(part, &["kind", "text", "language"])
                && text(&part["text"], 2048)
                && part.get("language").is_none_or(|v| text(v, 32))
        }
        Some("list") => {
            shape(part, &["kind", "items"]) && items(&part["items"], 32, |value| text(value, 2048))
        }
        Some("checklist") => {
            shape(part, &["kind", "items"]) && items(&part["items"], 32, checklist)
        }
        Some("timeline") => shape(part, &["kind", "items"]) && items(&part["items"], 32, milestone),
        _ => false,
    }
}

fn slide(value: &Json) -> bool {
    shape(value, &["heading", "points"])
        && text(&value["heading"], 800)
        && value["points"]
            .as_array()
            .is_some_and(|points| points.len() <= 32 && points.iter().all(|p| text(p, 800)))
}

fn flow_step(value: &Json) -> bool {
    shape(value, &["id", "label", "state", "description", "metadata"])
        && text(&value["label"], 2048)
        && step_state(&value["state"])
        && ["description", "metadata", "id"]
            .iter()
            .all(|key| value.get(key).is_none_or(|v| text(v, 2048)))
}

fn simulation(data: &Json) -> bool {
    if !shape(data, &["baseRevenue", "baseMarginPct", "variables"])
        || !data["baseRevenue"]
            .as_f64()
            .is_some_and(|v| (0.0..=1e18).contains(&v))
        || !data["baseMarginPct"]
            .as_f64()
            .is_some_and(|v| (-100.0..=100.0).contains(&v))
    {
        return false;
    }
    let mut keys = std::collections::HashSet::new();
    data["variables"].as_array().is_some_and(|rows| {
        !rows.is_empty()
            && rows.len() <= 2
            && rows.iter().all(|item| {
                let (Some(key), Some(min), Some(max), Some(initial)) = (
                    item["key"].as_str(),
                    item["min"].as_f64(),
                    item["max"].as_f64(),
                    item["initial"].as_f64(),
                ) else {
                    return false;
                };
                shape(
                    item,
                    &["key", "label", "min", "max", "step", "initial", "unit"],
                ) && matches!(key, "volume_growth" | "discount")
                    && keys.insert(key)
                    && text(&item["label"], 256)
                    && min < max
                    && (min..=max).contains(&initial)
                    && min >= -100.
                    && max <= if key == "discount" { 100. } else { 10000. }
                    && item
                        .get("step")
                        .is_none_or(|v| v.as_f64().is_some_and(|s| s > 0.))
                    && item.get("unit").is_none_or(|v| text(v, 32))
            })
    })
}

/// Returns true for authored material, for which the model cannot supply ERP provenance.
pub(super) fn authored(kind: &str, data: &Json) -> Result<bool> {
    let valid = match kind {
        "document" | "brief" => shape(data, &["sections"]) && items(&data["sections"], 32, section),
        "slides" => shape(data, &["slides"]) && items(&data["slides"], 40, slide),
        "checklist" => shape(data, &["items"]) && items(&data["items"], 32, checklist),
        "timeline" => shape(data, &["items"]) && items(&data["items"], 32, milestone),
        "flow" => shape(data, &["steps"]) && items(&data["steps"], 32, flow_step),
        "simulation" => simulation(data),
        "chart" | "table" | "pivot" | "dashboard" => return Ok(false),
        _ => return Err(Error::validation("Unknown canvas type.")),
    };
    if valid {
        Ok(true)
    } else {
        Err(Error::validation(format!(
            "Invalid {kind} canvas content. Use the declared section types, labels and item bounds."
        )))
    }
}
