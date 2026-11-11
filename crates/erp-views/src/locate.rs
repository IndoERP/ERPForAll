//! Locating the node a spec designates and the
//! translated-selector rule.

use crate::arch::{Arch, NodeId};
use crate::error::{ViewError, ViewResult};
use crate::limits::{Budget, ViewLimits};
use crate::xpath::{XPath, is_translated_attr};

/// The first node of `source` (document order) designated by the spec node `spec_id` of `spec`:
/// `<xpath expr>` by its expression, `<field>` by `name` only, any other tag by tag and every spec
/// attribute except `position`. `None` when nothing matches.
///
/// Every node visited costs one step of `budget` ([`crate::LimitKind::EvaluationSteps`]), every
/// tag, attribute name and value compared its own steps (see [`Budget::str_eq`]).
pub(crate) fn locate(
    source: &Arch,
    spec: &Arch,
    spec_id: NodeId,
    limits: &ViewLimits,
    budget: &mut Budget,
) -> ViewResult<Option<NodeId>> {
    let tag = spec.tag(spec_id);
    let checked = spec.attr(spec_id, "position").is_some();
    if tag == "xpath" {
        let expr = spec
            .attr(spec_id, "expr")
            .ok_or(ViewError::MissingXpathExpr)?;
        let xpath = XPath::parse(expr, limits)?;
        if checked && let Some(attr) = xpath.translated_selector() {
            return Err(ViewError::TranslatedAttributeSelector(attr.to_owned()));
        }
        return xpath.first(source, budget);
    }
    if checked
        && let Some((attr, _)) = spec
            .attrs(spec_id)
            .iter()
            .find(|(k, v)| !v.is_empty() && is_translated_attr(k))
    {
        return Err(ViewError::TranslatedAttributeSelector(attr.clone()));
    }
    if tag == "field" {
        let name = spec
            .attr(spec_id, "name")
            .ok_or_else(|| ViewError::FieldSpecWithoutName(spec_open_tag(spec, spec_id)))?;
        return first_element(source, budget, |id, budget| {
            Ok(budget.str_eq(source.tag(id), "field")?
                && attr_equals(source, id, "name", name, budget)?)
        });
    }
    first_element(source, budget, |id, budget| {
        if !budget.str_eq(source.tag(id), tag)? {
            return Ok(false);
        }
        for (k, v) in spec.attrs(spec_id) {
            if k != "position" && !attr_equals(source, id, k, v, budget)? {
                return Ok(false);
            }
        }
        Ok(true)
    })
}

/// `source.attr(id, name) == Some(value)`, charged: the attribute scan and the value comparison.
fn attr_equals(
    source: &Arch,
    id: NodeId,
    name: &str,
    value: &str,
    budget: &mut Budget,
) -> ViewResult<bool> {
    match source.attr_charged(id, name, budget)? {
        Some(have) => budget.str_eq(have, value),
        None => Ok(false),
    }
}

/// The first element of `source` in document order that satisfies `wanted`: one budget step per
/// node visited, `wanted` charges its own comparisons.
fn first_element(
    source: &Arch,
    budget: &mut Budget,
    mut wanted: impl FnMut(NodeId, &mut Budget) -> ViewResult<bool>,
) -> ViewResult<Option<NodeId>> {
    for (id, _) in source.walk(source.root_id()) {
        budget.charge()?;
        if !source.is_sentinel(id) && wanted(id, budget)? {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

pub(crate) fn spec_open_tag(spec: &Arch, spec_id: NodeId) -> String {
    let mut out = format!("<{}", spec.tag(spec_id));
    for (k, v) in spec.attrs(spec_id) {
        if k == "position" {
            continue;
        }
        out.push(' ');
        out.push_str(k);
        out.push_str("=\"");
        for c in v.chars() {
            match c {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '"' => out.push_str("&#34;"),
                '\'' => out.push_str("&#39;"),
                c => out.push(c),
            }
        }
        out.push('"');
    }
    out.push('>');
    out
}
