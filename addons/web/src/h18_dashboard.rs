// SPDX-License-Identifier: MIT OR Apache-2.0
//! Pure dashboard declaration admission; caller authorization remains in the H18 host.
use super::*;

fn attribute<'a>(node: impl Node<'a>, key: &str) -> ViewResult<&'a str> {
    node.attr(key)
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| invalid(format!("Dashboard item requires '{key}'.")))
}
pub(super) fn build<'a>(
    node: impl Node<'a>,
    spec: &ModelSpec,
    object: &mut erp_sdk::views::DescriptionObjectBuilder<'_>,
    budget: &DescriptionBudget,
) -> ViewResult<()> {
    // Every production CheckedViewInput was admitted from the original dashboard before pruning.
    object.set("version", |v| v.integer(1))?;
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    for item in node.children() {
        hash.update(item.tag().as_bytes());
        for key in [
            "name",
            "string",
            "domain",
            "filter",
            "color",
            "field",
            "aggregate",
            "date_field",
        ] {
            if let Some(value) = item.attr(key) {
                hash.update([1]);
                hash.update((value.len() as u64).to_le_bytes());
                hash.update(value.as_bytes());
            } else {
                hash.update([0]);
            }
        }
    }
    let _revision_storage = budget.reserve(64)?;
    let mut revision = String::with_capacity(64);
    for byte in hash.finalize() {
        use std::fmt::Write;
        write!(&mut revision, "{byte:02x}")
            .map_err(|_| invalid("Dashboard revision encoding failed."))?;
    }
    object.set("revision", |v| v.text(&revision))?;
    let mut source = node.children();
    object.set("items", |v| {
        v.array(node.children().count(), |_, v| {
            let item = source
                .next()
                .ok_or_else(|| invalid("Dashboard declaration changed during construction."))?;
            v.variant(item.tag(), |v| {
                v.object(|o| {
                    o.set("name", |v| v.text(attribute(item, "name")?))?;
                    o.set("label", |v| v.text(attribute(item, "string")?))?;
                    if let Some(domain) = item.attr("domain") {
                        o.set("domain", |v| source_expression(domain, v, |_| Ok(())))?;
                    }
                    if item.tag() == "count" {
                        for key in ["filter", "color"] {
                            if let Some(value) = item.attr(key) {
                                o.set(key, |v| v.text(value))?;
                            }
                        }
                    } else {
                        for key in ["field", "date_field"] {
                            o.set(key, |v| v.field_handle(spec.name, attribute(item, key)?))?;
                        }
                        o.set("aggregate", |v| v.text(attribute(item, "aggregate")?))?;
                    }
                    Ok(())
                })
            })
        })
    })?;
    Ok(())
}
