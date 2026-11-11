//! Extended section lowering through the same closed, request-budgeted H18 builder.
use super::{Node, boolean, invalid};
use erp_sdk::views::{DescriptionObjectBuilder, DescriptionValueBuilder, ViewResult};
use erp_sdk::{FieldSpec, FieldType, ModelSpec};

fn failure(id: usize, reason: &str) -> erp_sdk::views::ViewError {
    invalid(format!("Search panel section {id} {reason}."))
}
fn locate<'a, N: Node<'a>>(
    node: N,
    inherited_dynamic: bool,
    found: &mut Option<N>,
    count: &mut usize,
    panel_dynamic: &mut bool,
) -> ViewResult<()> {
    if matches!(node.attr("invisible"), Some("1" | "true" | "True")) {
        return Ok(());
    }
    let dynamic = inherited_dynamic || boolean(node, "invisible", false).is_err();
    if node.tag() == "searchpanel" {
        *panel_dynamic |= dynamic;
        *count = count
            .checked_add(1)
            .ok_or_else(|| failure(0, "exceeds panel count limit"))?;
        *found = Some(node);
        return Ok(());
    }
    for child in node.children() {
        locate(child, dynamic, found, count, panel_dynamic)?;
    }
    Ok(())
}
fn category<'a>(node: impl Node<'a>, id: usize) -> ViewResult<bool> {
    match node.attr("select") {
        None | Some("one") => Ok(true),
        Some("multi") => Ok(false),
        Some(_) => Err(failure(id, "has an unsupported selection mode")),
    }
}
fn field<'a>(spec: &'a ModelSpec, name: Option<&str>, id: usize) -> ViewResult<&'a FieldSpec> {
    name.and_then(|name| spec.field(name))
        .ok_or_else(|| failure(id, "references an unknown field"))
}
fn applicable<'a>(node: impl Node<'a>) -> impl Iterator<Item = &'a str> {
    // Absent applicability is the standard list/kanban pair.
    node.attr("view_types")
        .unwrap_or("list,kanban")
        .split(',')
        .map(str::trim)
}
fn flag<'a>(node: impl Node<'a>, name: &str, default: bool, id: usize) -> ViewResult<bool> {
    boolean(node, name, default).map_err(|_| {
        failure(
            id,
            "has an unsupported dynamic boolean; additional syntax requires a later release",
        )
    })
}
fn supported_field<'a>(
    spec: &'a ModelSpec,
    node: impl Node<'a>,
    id: usize,
    budget: &erp_sdk::views::DescriptionBudget,
) -> ViewResult<&'a FieldSpec> {
    let field = field(spec, node.attr("name"), id)?;
    if !field.store {
        return Err(failure(id, "requires a stored field"));
    }
    match &field.ty {
        FieldType::Selection {
            values: Some(choices),
            ..
        } => {
            if choices.is_empty() {
                return Err(failure(id, "requires static selection choices"));
            }
            let bytes = choices
                .len()
                .checked_mul(std::mem::size_of::<&str>())
                .ok_or_else(|| failure(id, "exceeds selection storage limit"))?;
            let mut storage = budget.reserve(bytes)?;
            let mut keys = Vec::new();
            keys.try_reserve_exact(choices.len())
                .map_err(|_| failure(id, "exceeds selection storage limit"))?;
            if keys.capacity() > choices.len() {
                storage.grow(
                    (keys.capacity() - choices.len())
                        .checked_mul(std::mem::size_of::<&str>())
                        .ok_or_else(|| failure(id, "exceeds selection storage limit"))?,
                )?;
            }
            for (key, _) in choices {
                if key.is_empty() || key.len() > 256 {
                    return Err(failure(id, "exceeds selection key byte limit"));
                }
                keys.push(key.as_str());
            }
            keys.sort_unstable();
            if keys.windows(2).any(|pair| pair[0] == pair[1]) {
                return Err(failure(id, "has duplicate selection keys"));
            }
        }
        FieldType::Many2one { .. } | FieldType::Many2many { .. } => {}
        _ => {
            return Err(failure(
                id,
                "has an unsupported field type; additional syntax requires a later release",
            ));
        }
    }
    Ok(field)
}
fn bounded_text<'a>(value: &'a str, max: usize, id: usize, kind: &str) -> ViewResult<&'a str> {
    if value.len() > max {
        return Err(failure(id, &format!("exceeds {kind} byte limit")));
    }
    if value.trim().is_empty() {
        return Err(failure(id, &format!("has an invalid {kind}")));
    }
    Ok(value)
}
fn icon(value: &str, id: usize) -> ViewResult<&str> {
    bounded_text(value, 64, id, "icon")?;
    if !value.as_bytes()[0].is_ascii_alphabetic()
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        return Err(failure(id, "has an invalid icon token"));
    }
    Ok(value)
}
fn limit<'a>(node: impl Node<'a>, id: usize) -> ViewResult<i64> {
    // Absent XML limit is the reference presentation default, independently from RPC None/zero.
    let raw = node.attr("limit").unwrap_or("200");
    let value = raw
        .trim()
        .parse::<u64>()
        .map_err(|_| failure(id, "has an invalid display limit"))?;
    if value > 9_007_199_254_740_991 {
        return Err(failure(id, "exceeds display limit"));
    }
    i64::try_from(value).map_err(|_| failure(id, "exceeds display limit"))
}
fn domain(
    source: &str,
    comodel: &str,
    id: usize,
    output: DescriptionValueBuilder<'_>,
) -> ViewResult<erp_sdk::views::DescriptionValue> {
    let limits = erp_sdk::expr::EvalLimits::default();
    let bound = erp_sdk::expr::source_preflight(source, limits)
        .map_err(|_| failure(id, "has an invalid domain"))?;
    let _scratch = output.budget().reserve(bound.reserve_bytes())?;
    let parsed = erp_sdk::expr::parse_with_limits(source, limits)
        .map_err(|_| failure(id, "has an invalid domain"))?;
    let ast = erp_sdk::expr::wire::ValidatedAst::from_expr(&parsed)
        .map_err(|_| failure(id, "has an invalid domain"))?;
    erp_sdk::views::validate_panel_domain(&ast).map_err(|_| {
        failure(
            id,
            "has an unsupported domain; additional syntax requires a later release",
        )
    })?;
    output.scoped_domain(comodel, &ast)
}

/// Return false only when the existing legacy selector owns this exact presentation.
pub(super) fn set<'a, N: Node<'a>>(
    object: &mut DescriptionObjectBuilder<'_>,
    root: N,
    spec: &'a ModelSpec,
    budget: &erp_sdk::views::DescriptionBudget,
) -> ViewResult<bool> {
    let mut selected = None;
    let mut total = 0;
    let mut panel_dynamic = false;
    locate(root, false, &mut selected, &mut total, &mut panel_dynamic)?;
    if total != 1 {
        return Ok(false);
    } // preserve the existing absent/duplicate-panel diagnostics
    let node = selected.ok_or_else(|| invalid("Search panel traversal changed."))?;
    let count = node.children().count();
    let only = node.children().next();
    let mut applicability = applicable(node);
    let default_applicability = matches!(
        (
            applicability.next(),
            applicability.next(),
            applicability.next()
        ),
        (Some("list"), Some("kanban"), None) | (Some("kanban"), Some("list"), None)
    );
    let extended = node.source_child_count() != 1
        || node.attr("class").is_some()
        || !default_applicability
        || only.is_some_and(|child| {
            child.attr("select") == Some("multi")
                || child.attr("icon").is_some()
                || child.attr("icon_class").is_some()
                || child.attr("depth").is_some()
                || child
                    .attr("name")
                    .and_then(|name| spec.field(name))
                    .is_some_and(|field| match &field.ty {
                        FieldType::Many2one { .. } | FieldType::Many2many { .. } => true,
                        FieldType::Selection {
                            values: Some(values),
                            ..
                        } => values.len() > 32,
                        _ => false,
                    })
        });
    if !extended {
        return Ok(false);
    }
    if panel_dynamic {
        return Err(failure(
            0,
            "has unsupported ancestor visibility; additional syntax requires a later release",
        ));
    }
    if count > 64 {
        return Err(failure(65, "exceeds section count limit"));
    }
    if node
        .attributes()
        .any(|(key, _)| !matches!(key, "view_types" | "invisible"))
    {
        return Err(failure(
            0,
            "has unsupported panel syntax; additional syntax requires a later release",
        ));
    }
    let view_count = applicable(node).count();
    if view_count == 0 || view_count > 64 {
        return Err(failure(0, "exceeds view key count limit"));
    }
    for (index, key) in applicable(node).enumerate() {
        if key.len() > 129 || key.split('.').any(|part| part.len() > 64) {
            return Err(failure(0, "exceeds view key byte limit"));
        }
        if !erp_sdk::views::is_valid_view_type_key(key) {
            return Err(failure(0, "has an invalid view key"));
        }
        if applicable(node).take(index).any(|prior| prior == key) {
            return Err(failure(0, "has duplicate view keys"));
        }
    }
    flag(node, "invisible", false, 0)?;
    for (index, child) in node.children().enumerate() {
        let id = child.source_ordinal()?;
        if child.tag() != "field" {
            return Err(failure(id, "requires a field declaration"));
        }
        if child.attributes().any(|(key, _)| {
            !matches!(
                key,
                "name"
                    | "string"
                    | "select"
                    | "icon"
                    | "enable_counters"
                    | "expand"
                    | "limit"
                    | "invisible"
                    | "hierarchize"
                    | "domain"
                    | "groupby"
            )
        }) {
            return Err(failure(
                id,
                "has unsupported section syntax; additional syntax requires a later release",
            ));
        }
        if flag(child, "invisible", false, id)? {
            continue;
        }
        let is_category = category(child, id)?;
        supported_field(spec, child, id, budget)?;
        if is_category
            && node.children().take(index).any(|previous| {
                previous.attr("name") == child.attr("name")
                    && matches!(previous.attr("select"), None | Some("one"))
                    && !matches!(previous.attr("invisible"), Some("1" | "true" | "True"))
            })
        {
            return Err(failure(id, "has an ambiguous category binding"));
        }
    }
    let visible_count = node
        .children()
        .filter(|child| !matches!(child.attr("invisible"), Some("1" | "true" | "True")))
        .count();
    if visible_count == 0 {
        return Ok(true);
    }
    object.set("search_panel", |value| value.variant("sections", |value| value.object(|panel| {
        panel.set("version", |value| value.integer(1))?;
        panel.set("view_types", |value| value.array(view_count, |index, value| value.text(applicable(node).nth(index).ok_or_else(|| failure(0, "lost applicability"))?)))?;
        panel.set("sections", |value| value.array(visible_count, |index, value| {
            let (_ordinal, child) = node.children().enumerate().filter(|(_, child)| !matches!(child.attr("invisible"), Some("1" | "true" | "True"))).nth(index).ok_or_else(|| failure(0, "lost a section"))?;
            let id = child.source_ordinal()?;
            let is_category = category(child, id)?;
            let field = supported_field(spec, child, id, value.budget())?;
            value.variant(if is_category { "category" } else { "filter" }, |value| value.object(|section| {
                section.set("id", |value| value.integer(i64::try_from(id).map_err(|_| failure(id, "has an invalid identity"))?))?;
                section.set("field", |value| value.field_handle(spec.name, field.name))?;
                section.set("label", |value| value.text(bounded_text(child.attr("string").unwrap_or(&field.string), 256, id, "label")?))?;
                section.set("icon", |value| value.text(icon(child.attr("icon").unwrap_or(if is_category { "folder" } else { "filter_alt" }), id)?))?;
                section.set("enable_counters", |value| value.boolean(flag(child, "enable_counters", false, id)?))?;
                section.set("expand", |value| value.boolean(flag(child, "expand", false, id)?))?;
                section.set("limit", |value| value.integer(limit(child, id)?))?;
                if is_category {
                    if child.attr("domain").is_some() || child.attr("groupby").is_some() { return Err(failure(id, "has unsupported category syntax; additional syntax requires a later release")); }
                    section.set("hierarchize", |value| value.boolean(flag(child, "hierarchize", true, id)?))?;
                } else {
                    let comodel = match &field.ty {
                        FieldType::Many2one { comodel, .. } | FieldType::Many2many { comodel, .. } => Some(comodel.as_str()),
                        _ => None,
                    };
                    if let Some(group) = child.attr("groupby") {
                        let model = comodel.ok_or_else(|| failure(id, "requires a relational grouping field"))?;
                        section.set("group_by", |value| value.field_handle(model, group))?;
                    }
                    if let Some(source) = child.attr("domain").filter(|source| !source.is_empty() && *source != "[]") {
                        let model = comodel.ok_or_else(|| failure(id, "requires a relational domain field"))?;
                        section.set("domain", |value| domain(source, model, id, value))?;
                    }
                }
                Ok(())
            }))
        }))?;
        Ok(())
    })))?;
    Ok(true)
}
