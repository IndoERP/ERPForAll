// SPDX-License-Identifier: MIT OR Apache-2.0
//! Kanban admission and lowering (design).
//!
//! The card is a closed declaration: `<templates><t t-name="card">` holding `<field>` nodes and at
//! most one each of `<main>`, `<aside>` and `<footer>`. These declarations select the readable
//! fields and their widgets. An optional `<restricted_templates>` sibling supplies custom markup
//! for the card through the pinned WASI compiler.

use super::{
    DescriptionBudget, DescriptionReservation, ModelSpec, Node, Plan, ViewResult, boolean,
    field_type_name, invalid, set_children, set_view_actions, widget,
};
use erp_sdk::FieldType;
use erp_sdk::expr::Expr;

/// The root attributes a kanban admits today; the others are refused by name.
pub(super) const ROOT_ATTRS: &[&str] = &[
    "string",
    "limit",
    "default_group_by",
    "group_scope",
    "quick_create",
    "records_draggable",
    "groups_draggable",
    "archivable",
    "can_open",
    "create",
    "edit",
    "delete",
];
const FLAGS: [&str; 4] = [
    "records_draggable",
    "groups_draggable",
    "archivable",
    "can_open",
];
/// Field kinds a kanban can open grouped by.
const GROUPABLE: [&str; 7] = [
    "boolean",
    "char",
    "integer",
    "selection",
    "many2one",
    "date",
    "datetime",
];
const MAX_COLORS: usize = 64;
const MAX_COLOR_BYTES: usize = 4096;
const MAX_COLOR_DEPTH: usize = 4;
const MAX_VALUE_BYTES: usize = 256;
const MARKUP: &str = "Put custom card markup inside <restricted_templates> under <kanban>.";

fn limit<'a>(node: impl Node<'a>) -> ViewResult<Option<i64>> {
    let Some(raw) = node.attr("limit") else {
        return Ok(None);
    };
    match raw.trim().parse::<i64>() {
        Ok(limit) if (1..=9_007_199_254_740_991).contains(&limit) => Ok(Some(limit)),
        _ => Err(invalid(format!(
            "Kanban limit \"{raw}\" must be a positive whole number."
        ))),
    }
}

fn group_by<'a, 's>(
    spec: &'s ModelSpec,
    node: impl Node<'a>,
) -> ViewResult<Option<&'s erp_sdk::FieldSpec>> {
    let Some(name) = node.attr("default_group_by") else {
        return Ok(None);
    };
    if name.contains(',') {
        return Err(invalid(format!(
            "Kanban default_group_by \"{name}\" names several fields; a kanban groups by one."
        )));
    }
    let field = spec.field(name.trim()).ok_or_else(|| {
        invalid(format!(
            "Kanban default_group_by names unknown field '{}.{name}'.",
            spec.name
        ))
    })?;
    let kind = field_type_name(field)?;
    if !GROUPABLE.contains(&kind) || !field.store {
        return Err(invalid(format!(
            "Kanban default_group_by '{}.{}' must be a stored boolean, char, integer, selection, many2one, date or datetime field.",
            spec.name, field.name
        )));
    }
    Ok(Some(field))
}

/// Validate the root attributes and the card count.
pub(super) fn root<'a, N: Node<'a>>(node: N, spec: &ModelSpec) -> ViewResult<()> {
    for flag in FLAGS.iter().chain(&["quick_create", "create"]) {
        boolean(node, flag, true)?;
    }
    limit(node)?;
    let grouping = group_by(spec, node)?;
    if let Some(scope) = node.attr("group_scope") {
        let relation = |field: &erp_sdk::FieldSpec| {
            field.store && matches!(field.ty, FieldType::Many2one { .. })
        };
        if !grouping.is_some_and(|field| relation(field) && field.name != scope)
            || !spec.field(scope).is_some_and(relation)
        {
            return Err(invalid(
                "Kanban group_scope and default_group_by must name distinct stored many2one fields.",
            ));
        }
    }
    if node.children().any(|child| child.tag() == "t") {
        return Err(invalid(
            "The kanban card template belongs inside <templates>.",
        ));
    }
    if node
        .children()
        .filter(|child| child.tag() == "restricted_templates")
        .count()
        > 1
    {
        return Err(invalid("A kanban has at most one restricted template set."));
    }
    let cards = node
        .children()
        .filter(|child| child.tag() == "templates")
        .flat_map(|templates| templates.children())
        .count();
    if cards != 1 {
        return Err(invalid(
            "A kanban view needs exactly one <t t-name=\"card\"> inside <templates>.",
        ));
    }
    if node
        .children()
        .filter(|child| child.tag() == "progressbar")
        .count()
        > 1
    {
        return Err(invalid("A kanban view has at most one <progressbar>."));
    }
    Ok(())
}

/// Classify a node below the kanban root; `parent` is the admitted parent (`<templates>` unwraps).
pub(super) fn classify<'a, N: Node<'a>>(
    node: N,
    parent: N,
    spec: &ModelSpec,
) -> ViewResult<&'static str> {
    let place = match parent.tag() {
        "kanban" => "view",
        "t" => "card",
        _ => "card region",
    };
    if parent.tag() == "kanban" && node.tag() == "restricted_templates" {
        super::attrs(node, &[])?;
        if !node
            .children()
            .any(|child| child.tag() == "t" && child.attr("t-name") == Some("card"))
        {
            return Err(invalid(
                "A restricted template set requires a template named card.",
            ));
        }
        return Ok("restricted_templates");
    }
    let allowed = matches!(
        (parent.tag(), node.tag()),
        ("kanban", "templates" | "t" | "progressbar")
            | ("t", "field" | "main" | "aside" | "footer")
            | ("main" | "aside" | "footer", "field")
    );
    if !allowed {
        return Err(invalid(match (parent.tag(), node.tag()) {
            ("kanban", "field") => "A <field> directly under <kanban> is unsupported. Declare its field inside <templates><t t-name=\"card\"> so restricted templates can project it.".to_owned(),
            (_, tag) => format!("<{tag}> is not allowed in a kanban {place}: a card holds <field> nodes and one each of <main>, <aside> and <footer>. {MARKUP}"),
        }));
    }
    match node.tag() {
        "templates" => {
            super::attrs(node, &[])?;
            Ok("unwrap")
        }
        "t" => {
            super::attrs(node, &["t-name"])?;
            if node.attr("t-name") != Some("card") {
                return Err(invalid(format!(
                    "Kanban template \"{}\" is not supported: the field declaration requires t-name=\"card\". {MARKUP}",
                    node.attr("t-name").unwrap_or("")
                )));
            }
            if ["main", "aside", "footer"].into_iter().any(|region| {
                node.children()
                    .filter(|child| child.tag() == region)
                    .count()
                    > 1
            }) {
                return Err(invalid(
                    "A kanban card declares each of <main>, <aside> and <footer> at most once.",
                ));
            }
            Ok("kanban_card")
        }
        "main" | "aside" | "footer" => {
            super::attrs(node, &[])?;
            Ok("kanban_section")
        }
        "progressbar" => {
            super::attrs(node, &["field", "colors", "sum_field", "help"])?;
            progress_field(spec, node)?;
            sum_field(spec, node)?;
            if node
                .attr("help")
                .is_some_and(|help| help.len() > super::MAX_LABEL)
            {
                return Err(invalid("Progress bar 'help' exceeds the 4096-byte limit."));
            }
            if node.children().next().is_some() {
                return Err(invalid("A <progressbar> has no children."));
            }
            Ok("kanban_progressbar")
        }
        _ => {
            super::attrs(
                node,
                &["name", "string", "widget", "options", "invisible", "groups"],
            )?;
            if node.children().next().is_some() {
                return Err(invalid("Field nodes cannot contain children."));
            }
            if widget(spec, node, Some(parent))? == "one2many" {
                return Err(invalid(format!(
                    "Kanban card field '{}.{}' is a one2many list, which a card cannot show.",
                    spec.name,
                    node.attr("name").unwrap_or("")
                )));
            }
            boolean(node, "invisible", false).map_err(|_| {
                invalid(
                    "Use a static value for 'invisible' on a kanban field declaration. A restricted template can conditionally display a field with t-if.",
                )
            })?;
            Ok("field")
        }
    }
}

fn progress_field<'a, 's>(
    spec: &'s ModelSpec,
    node: impl Node<'a>,
) -> ViewResult<&'s erp_sdk::FieldSpec> {
    let name = node
        .attr("field")
        .ok_or_else(|| invalid("A <progressbar> needs a field."))?;
    let field = spec.field(name).ok_or_else(|| {
        invalid(format!(
            "Progress bar names unknown field '{}.{name}'.",
            spec.name
        ))
    })?;
    if !matches!(field_type_name(field)?, "selection" | "char") {
        return Err(invalid(format!(
            "Progress bar field '{}.{}' must be a selection or char field.",
            spec.name, field.name
        )));
    }
    Ok(field)
}

fn sum_field<'a, 's>(
    spec: &'s ModelSpec,
    node: impl Node<'a>,
) -> ViewResult<Option<&'s erp_sdk::FieldSpec>> {
    let Some(name) = node.attr("sum_field") else {
        return Ok(None);
    };
    let field = spec.field(name).ok_or_else(|| {
        invalid(format!(
            "Progress bar sum_field names unknown field '{}.{name}'.",
            spec.name
        ))
    })?;
    if !matches!(field_type_name(field)?, "integer" | "float" | "monetary") {
        return Err(invalid(format!(
            "Progress bar sum_field '{}.{}' must be an integer, float or monetary field.",
            spec.name, field.name
        )));
    }
    Ok(Some(field))
}

/// Parse and check `colors` (a literal dictionary value → tone); returns the parsed dictionary with
/// the scratch reservation that pays for it.
pub(super) fn colors<'a>(
    node: impl Node<'a>,
    spec: &ModelSpec,
    budget: &DescriptionBudget,
) -> ViewResult<(DescriptionReservation, Expr)> {
    use erp_sdk::expr::{ExprError, Literal};
    let source = node
        .attr("colors")
        .ok_or_else(|| invalid("A <progressbar> needs colors."))?;
    if source.len() > MAX_COLOR_BYTES {
        return Err(invalid("Progress bar colors exceed 4096 UTF-8 bytes."));
    }
    let limits = erp_sdk::expr::EvalLimits::default()
        .with_max_source_bytes(MAX_COLOR_BYTES)
        .with_max_depth(MAX_COLOR_DEPTH);
    let parse_error = |error: ExprError| match error {
        ExprError::LimitExceeded { .. } => {
            invalid("Progress bar colors exceed the supported syntax bounds.")
        }
        other => invalid(format!("Invalid progress bar colors: {other}")),
    };
    let bound = erp_sdk::expr::source_preflight(source, limits).map_err(parse_error)?;
    let scratch = budget.reserve(bound.reserve_bytes())?;
    let parsed = erp_sdk::expr::parse_with_limits(source, limits).map_err(parse_error)?;
    let Expr::Dict(entries) = &parsed else {
        return Err(invalid("Progress bar colors must be a literal dictionary."));
    };
    if entries.len() > MAX_COLORS {
        return Err(invalid("Progress bar colors declare more than 64 values."));
    }
    let field = progress_field(spec, node)?;
    for (index, (key, tone)) in entries.iter().enumerate() {
        let (Expr::Literal(Literal::Str(key)), Expr::Literal(Literal::Str(tone))) = (key, tone)
        else {
            return Err(invalid(
                "Progress bar color keys and values must be string literals.",
            ));
        };
        let key = key.as_str();
        if key.is_empty() || key.len() > MAX_VALUE_BYTES {
            return Err(invalid(
                "Progress bar color keys must be 1 to 256 bytes long.",
            ));
        }
        tone_of(tone.as_str())?;
        if let FieldType::Selection {
            values: Some(values),
            ..
        } = &field.ty
            && !values.iter().any(|(value, _)| value == key)
        {
            return Err(invalid(format!(
                "Progress bar color '{key}' is not a value of selection field '{}.{}'.",
                spec.name, field.name
            )));
        }
        if entries.iter().take(index).any(
            |(prior, _)| matches!(prior, Expr::Literal(Literal::Str(prior)) if prior.as_str() == key),
        ) {
            return Err(invalid(format!(
                "Progress bar color '{key}' is declared twice."
            )));
        }
    }
    Ok((scratch, parsed))
}

fn tone_of(tone: &str) -> ViewResult<&'static str> {
    Ok(match tone {
        "success" => "success",
        "warning" => "warning",
        "danger" => "danger",
        "info" => "info",
        "muted" => "muted",
        other => {
            return Err(invalid(format!(
                "Progress bar color \"{other}\" must be one of success, warning, danger, info or muted."
            )));
        }
    })
}

/// Write the members of a kanban node kind.
pub(super) fn build<'a, N: Node<'a>>(
    plan: &Plan<N>,
    index: usize,
    node: N,
    spec: &ModelSpec,
    kind: &str,
    object: &mut erp_sdk::views::DescriptionObjectBuilder<'_>,
) -> ViewResult<()> {
    use erp_sdk::expr::Literal;
    match kind {
        "kanban" => {
            set_children(object, plan, index)?;
            if let Some(limit) = limit(node)? {
                object.set("page_limit", |value| value.integer(limit))?;
            }
            if let Some(field) = group_by(spec, node)? {
                object.set("default_group_by", |value| {
                    value.field_handle(spec.name, field.name)
                })?;
            }
            if let Some(scope) = node.attr("group_scope") {
                object.set("group_scope", |value| value.field_handle(spec.name, scope))?;
            }
            let quick = boolean(node, "quick_create", true)? && boolean(node, "create", true)?;
            object.set("quick_create", |value| value.boolean(quick))?;
            for flag in FLAGS {
                object.set(flag, |value| value.boolean(boolean(node, flag, true)?))?;
            }
            set_view_actions(object, node)
        }
        "kanban_card" => set_children(object, plan, index),
        "kanban_section" => {
            object.set("region", |value| value.text(node.tag()))?;
            set_children(object, plan, index)
        }
        "kanban_progressbar" => {
            let field = progress_field(spec, node)?;
            object.set("field", |value| value.field_handle(spec.name, field.name))?;
            object.set("colors", |value| {
                let (_scratch, parsed) = colors(node, spec, value.budget())?;
                let Expr::Dict(entries) = &parsed else {
                    return Err(invalid("Progress bar colors must be a literal dictionary."));
                };
                value.array(entries.len(), |position, value| {
                    let Some((
                        Expr::Literal(Literal::Str(key)),
                        Expr::Literal(Literal::Str(tone)),
                    )) = entries.get(position)
                    else {
                        return Err(invalid("Progress bar colors changed while lowering."));
                    };
                    value.object(|object| {
                        object.set("value", |value| value.text(key.as_str()))?;
                        object.set("tone", |value| value.text(tone_of(tone.as_str())?))
                    })
                })
            })?;
            if let Some(sum) = sum_field(spec, node)? {
                object.set("sum_field", |value| value.field_handle(spec.name, sum.name))?;
            }
            if let Some(help) = node.attr("help") {
                object.set("help", |value| value.text(help))?;
            }
            Ok(())
        }
        _ => Err(invalid("Unknown kanban node kind.")),
    }
}
