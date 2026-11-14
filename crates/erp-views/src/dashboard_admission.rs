// SPDX-License-Identifier: MIT OR Apache-2.0
//! Original dashboard syntax admission, before any policy can hide its nodes.
use crate::{NodeRef, ViewError, ViewResult};
use erp_registry::ModelSpec;

fn fail(message: impl Into<String>) -> ViewError {
    ViewError::InvalidDescription(message.into())
}
fn attrs(node: NodeRef<'_>, keys: &[&str]) -> ViewResult<()> {
    for (key, _) in node.attributes() {
        if !keys.contains(&key) {
            return Err(fail(format!("Unsupported dashboard attribute '{key}'.")));
        }
    }
    Ok(())
}
/// Validate original dashboard placement, vocabulary and scalar field kinds before access pruning.
///
/// This is structural/semantic admission only, never permission to read any referenced field.
/// # Errors
/// Invalid placement, unknown input, invalid labels/identities, unsupported types or bounds.
pub fn validate_dashboard(node: NodeRef<'_>, spec: &ModelSpec) -> ViewResult<()> {
    attrs(node, &[])?;
    if !node.text().trim().is_empty() {
        return Err(fail("Dashboard content must contain metric elements only."));
    }
    let parent = node.parent();
    if parent.is_some_and(|p| p.tag() == "kanban" && p.parent().is_none()) {
        return Err(fail(
            "A dashboard on a kanban view is not available yet (task \
             a later release); place it directly under the list root.",
        ));
    }
    if !parent.is_some_and(|p| p.tag() == "list" && p.parent().is_none()) {
        return Err(fail(
            "Place the dashboard directly under the list root, not inside another element.",
        ));
    }
    let total = node.children().count();
    if total == 0 {
        return Err(fail("Dashboard requires at least one item."));
    }
    if total > 32 {
        return Err(fail("Dashboard exceeds the 32-item admission bound."));
    }
    for (index, item) in node.children().enumerate() {
        if !item.text().trim().is_empty() || !item.tail().trim().is_empty() {
            return Err(fail("Dashboard content must contain metric elements only."));
        }
        let name = item
            .attr("name")
            .filter(|v| !v.is_empty())
            .ok_or_else(|| fail("Dashboard item requires 'name'."))?;
        if name.len() > 256 || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return Err(fail(
                "Dashboard item name must contain at most 256 letters, digits or underscores.",
            ));
        }
        let label = item
            .attr("string")
            .ok_or_else(|| fail("Dashboard item requires 'string'."))?;
        if label.trim().is_empty() || label.len() > 4096 {
            return Err(fail(
                "Dashboard label must be nonempty and at most 4096 bytes.",
            ));
        }
        if item.children().next().is_some() {
            return Err(fail(format!(
                "Dashboard item '{name}' cannot contain children."
            )));
        }
        if node
            .children()
            .take(index)
            .any(|v| v.attr("name") == Some(name))
        {
            return Err(fail(format!("Duplicate dashboard item '{name}'.")));
        }
        match item.tag() {
            "count" => {
                attrs(item, &["name", "string", "domain", "filter", "color"])?;
                if item.attr("domain").is_some() == item.attr("filter").is_some()
                    || item
                        .attr("filter")
                        .is_some_and(|v| v.trim().is_empty() || v.len() > 256)
                {
                    return Err(fail(format!(
                        "Dashboard count '{name}' requires exactly one domain or named filter."
                    )));
                }
                if let Some(color) = item.attr("color")
                    && !matches!(color, "info" | "success" | "warning" | "danger")
                {
                    return Err(fail(format!(
                        "Dashboard count '{name}' has invalid color '{color}'."
                    )));
                }
            }
            "kpi" => {
                attrs(
                    item,
                    &[
                        "name",
                        "string",
                        "field",
                        "aggregate",
                        "date_field",
                        "domain",
                    ],
                )?;
                let aggregate = item
                    .attr("aggregate")
                    .ok_or_else(|| fail("Dashboard item requires 'aggregate'."))?;
                if !matches!(aggregate, "sum" | "avg" | "min" | "max") {
                    return Err(fail(format!(
                        "Dashboard KPI '{name}' has unsupported aggregate '{aggregate}'."
                    )));
                }
                for key in ["field", "date_field"] {
                    let path = item
                        .attr(key)
                        .ok_or_else(|| fail(format!("Dashboard item requires '{key}'.")))?;
                    let valid = spec.field(path).is_some_and(|field| {
                        field.store
                            && if key == "field" {
                                matches!(field.ty.type_name(), "integer" | "float")
                            } else {
                                matches!(field.ty.type_name(), "date" | "datetime")
                            }
                    });
                    if !valid {
                        return Err(fail(format!(
                            "Dashboard KPI '{name}' requires a stored scalar {key} '{}.{path}' of the declared kind.",
                            spec.name
                        )));
                    }
                }
            }
            other => return Err(fail(format!("Unknown dashboard item <{other}>."))),
        }
    }
    Ok(())
}
