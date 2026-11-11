//! Pure builtin H18 lowering from checked nodes into the schema-directed construction path.
//!
//! The private node abstraction has only one production implementation, `CheckedNode`. Tests use
//! a raw bounded parser projection to test grammar/schema construction without minting authority.

use erp_sdk::views::{
    CheckedNode, CheckedViewInput, DescriptionBudget, DescriptionReservation, DescriptionValue,
    DescriptionValueBuilder, ViewError, ViewResult,
};
use erp_sdk::{FieldSpec, FieldType, ModelSpec};
use std::mem::size_of;

#[path = "h18_panel.rs"]
mod sections;

const MAX_NODES: usize = 4096;
const MAX_DEPTH: usize = 32;
const MAX_LABEL: usize = 4096;

fn invalid(message: impl Into<String>) -> ViewError {
    ViewError::InvalidDescription(message.into())
}
/// SearchModel's implemented scalar field-operator contract, separate from filter-domain syntax.
/// This pure check is repeated by the host on the final emitted wire slot, including custom builders.
pub(crate) fn validate_search_operator(model: &str, field: &str, operator: &str) -> ViewResult<()> {
    match operator {
        "ilike" | "=" | "!=" | ">" | "<" | ">=" | "<=" => Ok(()),
        _ => Err(invalid(format!(
            "Unsupported search field operator '{operator}' for '{model}.{field}'. Additional operators require a later release."
        ))),
    }
}
fn layout_error(message: impl std::fmt::Display) -> ViewError {
    invalid(format!(
        "{message} Unsupported form layout requires a later release."
    ))
}

trait Node<'a>: Copy {
    fn tag(self) -> &'a str;
    fn attr(self, key: &str) -> Option<&'a str>;
    fn attributes(self) -> impl Iterator<Item = (&'a str, &'a str)> + 'a;
    fn children(self) -> impl Iterator<Item = Self> + 'a;
    fn content_text(self) -> impl Iterator<Item = ViewResult<&'a str>> + 'a;
    fn is_unwrapped(self) -> bool;
    fn source_ordinal(self) -> ViewResult<usize>;
    fn source_child_count(self) -> usize;
}
impl<'a> Node<'a> for CheckedNode<'a> {
    fn source_child_count(self) -> usize {
        CheckedNode::source_child_count(&self)
    }
    fn source_ordinal(self) -> ViewResult<usize> {
        self.source_sibling_ordinal()
    }
    fn tag(self) -> &'a str {
        CheckedNode::tag(&self)
    }
    fn attr(self, key: &str) -> Option<&'a str> {
        CheckedNode::attr(&self, key)
    }
    fn attributes(self) -> impl Iterator<Item = (&'a str, &'a str)> + 'a {
        CheckedNode::attributes(&self)
    }
    fn children(self) -> impl Iterator<Item = Self> + 'a {
        CheckedNode::children(&self)
    }
    fn content_text(self) -> impl Iterator<Item = ViewResult<&'a str>> + 'a {
        self.content().map(|part| match part {
            erp_sdk::views::CheckedContent::Text(text) => Ok(text),
            erp_sdk::views::CheckedContent::Element(_) => Ok(""),
            // A new content kind must get an explicit policy before this grammar consumes it.
            _ => Err(invalid("Unknown checked content kind.")),
        })
    }
    fn is_unwrapped(self) -> bool {
        CheckedNode::is_unwrapped(&self)
    }
}
#[cfg(test)]
impl<'a> Node<'a> for erp_sdk::views::NodeRef<'a> {
    fn source_child_count(self) -> usize {
        self.children().count()
    }
    fn source_ordinal(self) -> ViewResult<usize> {
        let Some(parent) = self.parent() else {
            return Ok(1);
        };
        parent
            .children()
            .position(|child| child.id() == self.id())
            .map(|index| index + 1)
            .ok_or_else(|| invalid("Raw fixture node is absent from its parent."))
    }
    fn tag(self) -> &'a str {
        erp_sdk::views::NodeRef::tag(&self)
    }
    fn attr(self, key: &str) -> Option<&'a str> {
        erp_sdk::views::NodeRef::attr(&self, key)
    }
    fn attributes(self) -> impl Iterator<Item = (&'a str, &'a str)> + 'a {
        erp_sdk::views::NodeRef::attributes(&self)
    }
    fn children(self) -> impl Iterator<Item = Self> + 'a {
        erp_sdk::views::NodeRef::children(&self)
    }
    fn content_text(self) -> impl Iterator<Item = ViewResult<&'a str>> + 'a {
        std::iter::once(self.text())
            .chain(erp_sdk::views::NodeRef::children(&self).map(|child| child.tail()))
            .map(Ok)
    }
    fn is_unwrapped(self) -> bool {
        false
    }
}

#[derive(Clone, Copy)]
struct Entry<N> {
    node: N,
    parent: usize,
    kind: &'static str,
    first: Option<usize>,
    last: Option<usize>,
    next: Option<usize>,
}
struct Plan<N> {
    entries: Vec<Entry<N>>,
    _storage: DescriptionReservation,
    _spare: Option<DescriptionReservation>,
}

fn attrs<'a>(node: impl Node<'a>, allowed: &[&str]) -> ViewResult<()> {
    for (key, _) in node.attributes() {
        if !allowed.contains(&key) {
            return Err(invalid(format!(
                "View attribute '{key}' is unsupported by the current human UI contract."
            )));
        }
    }
    node.content_text().try_for_each(|text| {
        if text?.trim().is_empty() {
            Ok(())
        } else {
            Err(invalid(format!(
                "Text content in <{}> is unsupported by the current human UI contract.",
                node.tag()
            )))
        }
    })?;
    Ok(())
}
fn boolean<'a>(node: impl Node<'a>, key: &str, default: bool) -> ViewResult<bool> {
    match node.attr(key) {
        None => Ok(default),
        Some("1" | "true" | "True") => Ok(true),
        Some("0" | "false" | "False") => Ok(false),
        Some(_) => Err(invalid(format!(
            "Dynamic '{key}' modifier is unsupported by the current human UI contract."
        ))),
    }
}
/// The five chatter flags, in wire order (addendum). `has_activities` is not one of them:
/// the server derives it from the model.
const CHATTER_FLAGS: [&str; 5] = [
    "reload_on_post",
    "reload_on_attachment",
    "reload_on_activity",
    "reload_on_follower",
    "open_attachments",
];
fn chatter_flag<'a>(node: impl Node<'a>, key: &str) -> ViewResult<bool> {
    match node.attr(key) {
        None | Some("0" | "false" | "False") => Ok(false),
        Some("1" | "true" | "True") => Ok(true),
        Some(_) => Err(invalid(format!(
            "Chatter flag '{key}' must be one of 1, 0, true, false, True or False."
        ))),
    }
}
fn tracks<'a>(node: impl Node<'a>, key: &str, default: u32) -> ViewResult<u32> {
    let Some(raw) = node.attr(key) else {
        return Ok(default);
    };
    let refuse = || {
        layout_error(format!(
            "<{}> {key} must be an even integer from 2 through 12.",
            node.tag()
        ))
    };
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(refuse());
    }
    let value = raw.parse::<u32>().map_err(|_| refuse())?;
    if !(2..=12).contains(&value) || value % 2 != 0 {
        return Err(refuse());
    }
    Ok(value)
}
fn count<'a>(node: impl Node<'a>, depth: usize, total: &mut usize) -> ViewResult<()> {
    if depth > MAX_DEPTH {
        return Err(invalid("View exceeds the 32-level depth limit."));
    }
    *total = total
        .checked_add(1)
        .ok_or_else(|| invalid("View node count overflow."))?;
    if *total > MAX_NODES {
        return Err(invalid("View exceeds the 4096-node limit."));
    }
    for child in node.children() {
        count(child, depth + 1, total)?;
    }
    Ok(())
}

fn field<'a, 's>(spec: &'s ModelSpec, node: impl Node<'a>) -> ViewResult<&'s FieldSpec> {
    let name = node
        .attr("name")
        .ok_or_else(|| invalid("Field node requires name."))?;
    spec.field(name).ok_or_else(|| {
        invalid(format!(
            "View references unknown field '{}.{name}'.",
            spec.name
        ))
    })
}
fn field_type_name(field: &FieldSpec) -> ViewResult<&'static str> {
    match &field.ty {
        FieldType::Id => Ok("integer"),
        FieldType::Char { .. }
        | FieldType::Text
        | FieldType::Boolean
        | FieldType::Integer
        | FieldType::Float { .. }
        | FieldType::Monetary { .. }
        | FieldType::Date
        | FieldType::Datetime
        | FieldType::Many2one { .. }
        | FieldType::One2many { .. }
        | FieldType::Many2many { .. } => Ok(field.ty.type_name()),
        FieldType::Selection {
            values: Some(_), ..
        } => Ok("selection"),
        FieldType::Selection { .. } => Err(invalid(format!(
            "Dynamic selection field '{}' is unsupported by the current human UI contract.",
            field.name
        ))),
        _ => Err(invalid(format!(
            "Field type '{}' has no registered human UI renderer.",
            field.ty.type_name()
        ))),
    }
}
fn widget<'a>(
    spec: &ModelSpec,
    node: impl Node<'a>,
    parent: Option<impl Node<'a>>,
) -> ViewResult<&'a str> {
    let field = field(spec, node)?;
    let kind = field_type_name(field)?;
    let key = match node.attr("widget") {
        Some(key) => key,
        None => kind,
    };
    let built_in = matches!(
        key,
        "char"
            | "text"
            | "boolean"
            | "integer"
            | "float"
            | "monetary"
            | "date"
            | "datetime"
            | "selection"
            | "many2one"
            | "one2many"
            | "many2many"
    );
    if built_in
        && key != kind
        && !(key == "float" && kind == "monetary")
        && !(key == "monetary" && matches!(kind, "float" | "integer"))
        && !(key == "char" && kind == "text")
    {
        return Err(invalid(format!(
            "Built-in widget '{key}' cannot render field '{}.{}' of type '{kind}'.",
            spec.name, field.name
        )));
    }
    let statistic = parent.is_some_and(|parent| parent.tag() == "button");
    if statistic {
        if !(key == "statinfo" && matches!(kind, "integer" | "float")
            || key == "relation_count" && kind == "one2many")
        {
            return Err(layout_error(
                "Smart-button fields require numeric statinfo or one2many relation_count values.",
            ));
        }
        if key == "relation_count"
            && (!field.store
                || field.compute.is_some()
                || field.related.is_some()
                || field.inherited.is_some()
                || field.domain.is_some()
                || field.context.is_some())
        {
            return Err(layout_error(
                "relation_count requires a direct stored one2many without a domain or field context.",
            ));
        }
        if ["required", "readonly", "force_save"]
            .iter()
            .any(|key| node.attr(key).is_some())
        {
            return Err(layout_error(
                "Smart-button statistics are readonly and cannot declare write modifiers.",
            ));
        }
    } else if !built_in {
        let mut found = None;
        for decl in erp_sdk::views::FIELD_WIDGETS {
            if decl.key == key {
                if found.is_some() {
                    return Err(invalid(format!(
                        "duplicate native field widget declaration '{key}'"
                    )));
                }
                found = Some(decl);
            }
        }
        let decl =
            found.ok_or_else(|| invalid(format!("Field widget '{key}' is not registered.")))?;
        if !decl.field_types.contains(&kind) {
            return Err(invalid(format!(
                "Field widget '{key}' does not support type '{kind}'."
            )));
        }
    }
    if key == "statusbar" {
        if !parent.is_some_and(|parent| parent.tag() == "header") {
            return Err(invalid(
                "The statusbar widget requires a record form header.",
            ));
        }
        let FieldType::Selection {
            values: Some(values),
            ..
        } = &field.ty
        else {
            return Err(invalid("The statusbar widget requires a selection field."));
        };
        if let Some(visible) = node.attr("statusbar_visible") {
            for (index, key) in visible.split(',').map(str::trim).enumerate() {
                if key.is_empty() {
                    return Err(invalid(
                        "statusbar_visible must contain nonempty selection keys.",
                    ));
                }
                if !values.iter().any(|(value, _)| value == key) {
                    return Err(invalid(format!(
                        "Unknown statusbar_visible selection key '{key}'."
                    )));
                }
                if visible
                    .split(',')
                    .take(index)
                    .any(|prior| prior.trim() == key)
                {
                    return Err(invalid(format!(
                        "Duplicate statusbar_visible selection key '{key}'."
                    )));
                }
            }
        }
    } else if node.attr("statusbar_visible").is_some() {
        return Err(invalid("statusbar_visible requires widget='statusbar'."));
    }
    Ok(key)
}

const MAX_CODE_OPTIONS_BYTES: usize = 4096;
const MAX_CODE_OPTIONS_DEPTH: usize = 8;

/// Admit source configuration without evaluating any part of it. Scratch belongs to the same
/// request as the plan and payload; the result contains only a closed, allocation-free mode.
fn code_mode(
    source: Option<&str>,
    budget: &DescriptionBudget,
) -> ViewResult<erp_sdk::views::wire::CodeMode> {
    use erp_sdk::expr::{Expr, ExprError, Literal};
    use erp_sdk::views::wire::CodeMode;
    let Some(source) = source else {
        return Ok(CodeMode::Qweb);
    };
    if source.len() > MAX_CODE_OPTIONS_BYTES {
        return Err(invalid("Code field options exceed 4096 UTF-8 bytes."));
    }
    let limits = erp_sdk::expr::EvalLimits::default()
        .with_max_source_bytes(MAX_CODE_OPTIONS_BYTES)
        .with_max_depth(MAX_CODE_OPTIONS_DEPTH);
    let parse_error = |error: ExprError| match error {
        ExprError::LimitExceeded { .. } => {
            invalid("Code field options exceed the supported syntax bounds.")
        }
        other => invalid(format!("Invalid code field options: {other}")),
    };
    let bound = erp_sdk::expr::source_preflight(source, limits).map_err(parse_error)?;
    let _scratch = budget.reserve(bound.reserve_bytes())?;
    let parsed = erp_sdk::expr::parse_with_limits(source, limits).map_err(parse_error)?;
    let Expr::Dict(entries) = &parsed else {
        return Err(invalid("Code field options must be a literal dictionary."));
    };
    let mut mode = None;
    for (key, value) in entries {
        let (Expr::Literal(Literal::Str(key)), Expr::Literal(Literal::Str(value))) = (key, value)
        else {
            return Err(invalid(
                "Code field option keys and values must be string literals.",
            ));
        };
        if key != "mode" {
            return Err(invalid(format!("Unknown code field option '{key}'.")));
        }
        if mode.is_some() {
            return Err(invalid("Duplicate code field option 'mode'."));
        }
        mode = Some(
            CodeMode::from_wire_str(value.as_str())
                .ok_or_else(|| invalid(format!("Unsupported code field mode '{value}'.")))?,
        );
    }
    // The absent-key default is an explicit adapter policy; a present invalid value never gets here.
    Ok(match mode {
        Some(mode) => mode,
        None => CodeMode::Qweb,
    })
}

fn classify<'a, N: Node<'a>>(
    node: N,
    parent: Option<N>,
    spec: &ModelSpec,
    key: &str,
    service: bool,
) -> ViewResult<&'static str> {
    let is_root = parent.is_none();
    if node.is_unwrapped() {
        attrs(node, &[])?;
        return Ok("unwrap");
    }
    if is_root {
        if node.tag() != key {
            return Err(layout_error(format!(
                "Expected <{key}> view root, found <{}>.",
                node.tag()
            )));
        }
        attrs(
            node,
            match key {
                "form" => &["record_mode", "string", "create", "edit", "delete"],
                "list" => &["string", "limit", "create", "edit", "delete"],
                "search" => &["string"],
                _ => return Err(invalid("Unknown builtin view type.")),
            },
        )?;
        return Ok(match key {
            "form" => "form",
            "list" => "list",
            "search" => "search",
            _ => return Err(invalid("Unknown builtin view type.")),
        });
    }
    let parent = parent.ok_or_else(|| invalid("Missing parent of a view node."))?;
    let allowed = match parent.tag() {
        "form" if service => node.tag() == "widget",
        "list" => node.tag() == "field",
        "search" => matches!(
            node.tag(),
            "field" | "filter" | "separator" | "group" | "searchpanel"
        ),
        "group" if key == "search" => matches!(node.tag(), "filter" | "separator"),
        "notebook" => node.tag() == "page",
        "header" => {
            node.tag() == "button"
                || node.tag() == "field" && node.attr("widget") == Some("statusbar")
        }
        "footer" => node.tag() == "button",
        "aside" => matches!(node.tag(), "field" | "group" | "separator" | "widget"),
        "div" if parent.attr("class") == Some("oe_title") => {
            matches!(node.tag(), "h1" | "h2" | "field")
        }
        "h1" | "h2" => node.tag() == "field",
        "div" if parent.attr("name") == Some("button_box") => node.tag() == "button",
        "button" if has_button_class(parent, "oe_stat_button") => node.tag() == "field",
        "form" | "sheet" | "group" | "page" => {
            matches!(
                node.tag(),
                "field" | "group" | "notebook" | "separator" | "widget"
            ) || node.tag() == "div" && node.attr("class") == Some("oe_title")
                || parent.tag() == "sheet"
                    && node.tag() == "div"
                    && node.attr("name") == Some("button_box")
                || parent.tag() == "form"
                    && matches!(
                        node.tag(),
                        "sheet" | "header" | "footer" | "aside" | "chatter"
                    )
        }
        _ => false,
    };
    if !allowed {
        return Err(layout_error(format!(
            "View element <{}> is not allowed under <{}>.",
            node.tag(),
            parent.tag()
        )));
    }
    for attr in ["name", "string"] {
        if node.attr(attr).is_some_and(|value| value.len() > MAX_LABEL) {
            return Err(layout_error(format!(
                "Layout '{attr}' exceeds the 4096-byte limit."
            )));
        }
    }
    match node.tag() {
        "field" if key == "search" => {
            attrs(
                node,
                &[
                    "name",
                    "string",
                    "filter_domain",
                    "operator",
                    "groups",
                    "invisible",
                ],
            )?;
            let field = field(spec, node)?;
            if let Some(operator) = node.attr("operator") {
                validate_search_operator(spec.name, field.name, operator)?;
            }
            if node.children().next().is_some() {
                return Err(invalid("Field nodes cannot contain children."));
            }
            Ok("search_field")
        }
        "field" => {
            attrs(
                node,
                &[
                    "name",
                    "string",
                    "widget",
                    "groups",
                    "options",
                    "required",
                    "readonly",
                    "invisible",
                    "force_save",
                    "statusbar_visible",
                    "placeholder",
                    "help",
                    "autocomplete",
                    "password",
                    "domain",
                    "context",
                    "nolabel",
                ],
            )?;
            boolean(node, "nolabel", false)?;
            if service {
                return Err(invalid("Service forms cannot contain field nodes."));
            }
            if node.children().next().is_some() {
                return Err(invalid("Field nodes cannot contain children."));
            }
            widget(spec, node, Some(parent))?;
            boolean(node, "force_save", false)?;
            // Record forms evaluate dynamic modifiers; list rows and x2many columns (which come from list views) do
            // not yet, so a dynamic expression there would be ignored. Refuse it (lead r1 M4).
            if key == "list" {
                for attr in ["required", "readonly", "invisible"] {
                    if boolean(node, attr, false).is_err() {
                        return Err(invalid(format!(
                            "Dynamic '{attr}' modifiers on list fields need per-row evaluation, which a later release adds."
                        )));
                    }
                }
            }
            Ok("field")
        }
        "filter" => {
            attrs(
                node,
                &["name", "string", "domain", "context", "groups", "invisible"],
            )?;
            if node.children().next().is_some() {
                return Err(invalid("Filter nodes cannot contain children."));
            }
            Ok("search_filter")
        }
        "separator" => {
            attrs(node, &["name", "string", "groups", "invisible"])?;
            if node.children().next().is_some() {
                return Err(invalid("Separator nodes cannot contain children."));
            }
            Ok(if key == "search" {
                "search_separator"
            } else {
                "separator"
            })
        }
        "group" if key == "search" => {
            attrs(node, &["name", "string", "expand", "groups", "invisible"])?;
            boolean(node, "expand", false)?;
            Ok("search_group")
        }
        "searchpanel" => Ok("searchpanel"),
        "widget" => {
            if !service {
                return validate_display_widget(node);
            }
            attrs(node, &["name", "scope_kinds", "app"])?;
            if node.children().next().is_some() {
                return Err(invalid("Service widgets cannot contain child nodes."));
            }
            match node.attr("name") {
                Some("web.addons") => {
                    if node.attr("scope_kinds").is_some() {
                        return Err(invalid("web.addons does not accept scope_kinds."));
                    }
                    if node.attr("app").is_some() {
                        return Err(invalid("web.addons does not accept app."));
                    }
                }
                Some("web.settings") => {
                    if let Some(app) = node.attr("app")
                        && !erp_sdk::views::wire::is_settings_app_name(app)
                    {
                        return Err(invalid(format!(
                            "web.settings app '{app}' is not an addon technical name."
                        )));
                    }
                    let scopes = node
                        .attr("scope_kinds")
                        .ok_or_else(|| invalid("web.settings requires scope_kinds."))?;
                    for (index, scope) in scopes.split(',').enumerate() {
                        if !matches!(scope, "global" | "company") {
                            return Err(invalid(format!(
                                "Unknown web.settings scope kind '{scope}'."
                            )));
                        }
                        if scopes.split(',').take(index).any(|prior| prior == scope) {
                            return Err(invalid(format!(
                                "Duplicate web.settings scope kind '{scope}'."
                            )));
                        }
                    }
                }
                Some(other) => {
                    return Err(invalid(format!(
                        "Service widget '{other}' is not registered."
                    )));
                }
                None => return Err(invalid("Service widget requires a name.")),
            }
            Ok("widget")
        }
        "button" => {
            validate_button(node, parent)?;
            Ok(if node.attr("special") == Some("cancel") {
                "cancel_button"
            } else if has_button_class(node, "oe_stat_button") {
                "stat_button"
            } else if node.attr("type") == Some("object") {
                "button"
            } else {
                "action_button"
            })
        }
        "chatter" => {
            attrs(node, &CHATTER_FLAGS)?;
            for flag in CHATTER_FLAGS {
                chatter_flag(node, flag)?;
            }
            if !spec.inherits("mail.thread") {
                return Err(invalid(format!(
                    "View element <chatter> requires model '{}' to inherit 'mail.thread'.",
                    spec.name
                )));
            }
            Ok("chatter")
        }
        "sheet" | "header" | "footer" | "aside" => {
            attrs(node, &["groups", "invisible"])?;
            Ok(match node.tag() {
                "sheet" => "sheet",
                "header" => "header",
                "footer" => "footer",
                _ => "aside",
            })
        }
        "div" if node.attr("class") == Some("oe_title") => {
            attrs(node, &["class", "name", "groups", "invisible"])?;
            Ok("title")
        }
        "div" if node.attr("name") == Some("button_box") => {
            attrs(node, &["name", "class", "groups", "invisible"])?;
            if node
                .attr("class")
                .is_some_and(|class| class != "oe_button_box")
            {
                return Err(layout_error(
                    "button_box accepts only class='oe_button_box'.",
                ));
            }
            Ok("button_box")
        }
        "h1" | "h2" => {
            attrs(node, &["name", "groups", "invisible"])?;
            Ok("heading")
        }
        "group" => {
            attrs(
                node,
                &["name", "string", "col", "colspan", "groups", "invisible"],
            )?;
            tracks(
                node,
                "col",
                if node.children().any(|child| child.tag() == "group") {
                    4
                } else {
                    2
                },
            )?;
            tracks(node, "colspan", 2)?;
            Ok("group")
        }
        "notebook" => {
            attrs(node, &["name", "groups", "invisible"])?;
            Ok("notebook")
        }
        "page" => {
            attrs(node, &["name", "string", "groups", "invisible"])?;
            page_name(node)?;
            Ok("page")
        }
        other => Err(invalid(format!(
            "View element <{other}> is unsupported by the current human UI contract."
        ))),
    }
}
fn page_name<'a>(node: impl Node<'a>) -> ViewResult<&'a str> {
    node.attr("name")
        .filter(|value| !value.trim().is_empty())
        .or_else(|| node.attr("string").filter(|value| !value.trim().is_empty()))
        .ok_or_else(|| layout_error("Page requires a nonempty name or string."))
}
fn button_tokens<'a>(node: impl Node<'a>) -> impl Iterator<Item = &'a str> {
    // Absent class means no declaration; ASCII spaces alone canonicalize to no tokens.
    node.attr("class")
        .unwrap_or("")
        .split(' ')
        .filter(|token| !token.is_empty())
}
fn has_button_class<'a>(node: impl Node<'a>, expected: &str) -> bool {
    button_tokens(node).any(|token| token == expected)
}
fn rank_class(token: &str) -> Option<&'static str> {
    match token {
        "btn-primary" | "oe_highlight" => Some("primary"),
        "btn-secondary" => Some("secondary"),
        "btn-link" => Some("link"),
        "btn-success" => Some("success"),
        "btn-info" => Some("info"),
        "btn-warning" => Some("warning"),
        "btn-danger" => Some("danger"),
        _ => None,
    }
}
fn presentation_class(token: &str) -> bool {
    rank_class(token).is_some() || matches!(token, "rounded-circle" | "oe_stat_button")
}
fn validate_button_classes<'a>(node: impl Node<'a>) -> ViewResult<()> {
    if node.attr("class").is_some_and(|raw| raw.len() > 512) || button_tokens(node).count() > 8 {
        return Err(invalid(
            "Button class exceeds its 512-byte/eight-token bound.",
        ));
    }
    for (index, token) in button_tokens(node).enumerate() {
        if !erp_sdk::views::wire::valid_button_class_token(token) {
            return Err(invalid("Invalid button class token."));
        }
        if button_tokens(node).take(index).any(|prior| prior == token) {
            return Err(invalid("Duplicate button class token."));
        }
    }
    Ok(())
}
fn button_label<'a>(node: impl Node<'a>) -> ViewResult<&'a str> {
    let label = if has_button_class(node, "oe_stat_button") && node.attr("string").is_none() {
        node.children()
            .find(|child| matches!(child.attr("widget"), Some("statinfo" | "relation_count")))
            .and_then(|child| child.attr("string"))
    } else {
        node.attr("string")
    };
    let label = label.unwrap_or(""); // Icon-only declarations use their explicit title as accessible name.
    if label.trim().is_empty()
        && node
            .attr("title")
            .is_none_or(|title| title.trim().is_empty())
    {
        return Err(layout_error("Button requires a nonempty string."));
    }
    if label.len() > MAX_LABEL {
        return Err(layout_error("Button 'string' exceeds the 4096-byte limit."));
    }
    Ok(label)
}
fn button_variant<'a>(node: impl Node<'a>) -> ViewResult<&'static str> {
    validate_button_classes(node)?;
    let mut rank = None;
    for next in button_tokens(node).filter_map(rank_class) {
        if rank.is_some_and(|prior| prior != next) {
            return Err(invalid("Conflicting button rank classes."));
        }
        rank = Some(next);
    }
    // No class keeps the existing default; structural-only smart buttons retain their legacy rank.
    Ok(rank.unwrap_or(
        if node.attr("class").is_none() || has_button_class(node, "oe_stat_button") {
            "secondary"
        } else {
            "unstyled"
        },
    ))
}
fn button_size<'a>(node: impl Node<'a>) -> ViewResult<&'static str> {
    match node.attr("size") {
        None | Some("md") => Ok("normal"),
        Some("sm") => Ok("small"),
        Some("lg") => Ok("large"),
        Some(other) => Err(invalid(format!("Invalid button size '{other}'."))),
    }
}
fn button_debounce<'a>(node: impl Node<'a>) -> ViewResult<i64> {
    match node.attr("debounce") {
        None => Ok(0),
        Some(raw) => raw
            .parse::<u32>()
            .ok()
            .filter(|value| *value <= 60000)
            .map(i64::from)
            .ok_or_else(|| invalid("Button debounce must be an integer from 0 to 60000.")),
    }
}
fn validate_button<'a>(node: impl Node<'a>, parent: impl Node<'a>) -> ViewResult<()> {
    attrs(
        node,
        &[
            "special",
            "name",
            "type",
            "href",
            "string",
            "class",
            "invisible",
            "confirm",
            "groups",
            "size",
            "icon",
            "title",
            "help",
            "data-hotkey",
            "debounce",
            "block-ui",
            "confirm-title",
            "confirm-label",
            "cancel-label",
            "context",
        ],
    )?;
    if node.attr("special").is_some() {
        if parent.tag() != "footer" || node.attr("special") != Some("cancel") {
            return Err(layout_error(
                "Only a footer button supports special='cancel'.",
            ));
        }
        for key in ["name", "type", "href", "confirm", "context"] {
            if node.attr(key).is_some() {
                return Err(layout_error(format!(
                    "A special='cancel' button cannot combine with '{key}'."
                )));
            }
        }
    } else {
        match node.attr("type") {
            Some("object") => {
                let name = node
                    .attr("name")
                    .ok_or_else(|| layout_error("Button requires a name."))?;
                if name.len() > 128
                    || !name.starts_with(|c: char| c.is_ascii_lowercase())
                    || !name
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
                {
                    return Err(layout_error(format!(
                        "Button name '{name}' is not a public method name."
                    )));
                }
                if node.attr("href").is_some() {
                    return Err(invalid("Object button cannot declare href."));
                }
            }
            Some("action") => {
                button_action_id(node)?;
                if node.attr("href").is_some() {
                    return Err(invalid("Action button cannot declare href."));
                }
            }
            Some("url") => {
                let href = node
                    .attr("href")
                    .ok_or_else(|| invalid("URL button requires href."))?;
                if !erp_sdk::views::wire::valid_button_url(href) {
                    return Err(invalid(format!("Invalid button URL '{href}'.")));
                }
                if node.attr("name").is_some() {
                    return Err(invalid("URL button cannot declare name."));
                }
            }
            None => {
                if node.attr("name").is_some() || node.attr("href").is_some() {
                    return Err(invalid("An inert button cannot declare name or href."));
                }
            }
            Some(other) => return Err(invalid(format!("Unsupported button type '{other}'."))),
        }
    }
    if (parent.tag() == "div") != has_button_class(node, "oe_stat_button")
        || has_button_class(node, "oe_stat_button") && node.attr("type") != Some("object")
    {
        return Err(layout_error(
            "oe_stat_button must be inside div[name='button_box']; that area accepts only smart buttons.",
        ));
    }
    for key in [
        "confirm",
        "confirm-title",
        "confirm-label",
        "cancel-label",
        "title",
        "help",
    ] {
        if let Some(value) = node.attr(key) {
            if value.len() > MAX_LABEL {
                return Err(invalid(format!(
                    "Button '{key}' exceeds the 4096-byte limit."
                )));
            }
            if matches!(
                key,
                "confirm" | "confirm-title" | "confirm-label" | "cancel-label"
            ) && value.trim().is_empty()
            {
                return Err(invalid(format!("Button '{key}' cannot be empty.")));
            }
            if matches!(key, "confirm-title" | "confirm-label" | "cancel-label")
                && node.attr("confirm").is_none()
            {
                return Err(invalid("Button confirmation labels require confirm."));
            }
        }
    }
    button_size(node)?;
    button_debounce(node)?;
    boolean(node, "block-ui", false)?;
    if let Some(key) = node.attr("data-hotkey")
        && !erp_sdk::views::wire::valid_button_hotkey(key)
    {
        return Err(invalid(format!("Invalid button hotkey '{key}'.")));
    }
    if let Some(icon) = node.attr("icon")
        && (icon.len() > 256
            || !icon.strip_prefix("lucide:").is_some_and(|name| {
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            }))
    {
        return Err(invalid("Invalid button icon identity."));
    }
    button_label(node)?;
    button_variant(node)?;
    Ok(())
}
fn button_action_id<'a>(node: impl Node<'a>) -> ViewResult<i64> {
    node.attr("name")
        .filter(|value| !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| (1..=9_007_199_254_740_991).contains(value))
        .and_then(|value| i64::try_from(value).ok())
        .ok_or_else(|| invalid("Button action identity must be a positive JSON-safe integer."))
}
fn ribbon_text<'a>(node: impl Node<'a>) -> ViewResult<&'a str> {
    node.attr("title")
        .filter(|text| !text.trim().is_empty())
        .or_else(|| node.attr("text").filter(|text| !text.trim().is_empty()))
        .ok_or_else(|| invalid("Ribbon requires nonempty title or text."))
}
fn ribbon_background<'a>(node: impl Node<'a>) -> ViewResult<&'static str> {
    match node.attr("bg_color") {
        None | Some("bg-success" | "text-bg-success") => Ok("success"),
        Some("bg-info" | "text-bg-info") => Ok("info"),
        Some("bg-warning" | "text-bg-warning") => Ok("warning"),
        Some("bg-danger" | "text-bg-danger") => Ok("danger"),
        Some(other) => Err(invalid(format!("Unsupported ribbon background '{other}'."))),
    }
}
fn validate_display_widget<'a>(node: impl Node<'a>) -> ViewResult<&'static str> {
    if node.children().next().is_some() {
        return Err(invalid("Display widgets cannot contain child nodes."));
    }
    match node.attr("name") {
        Some("web_ribbon") => {
            attrs(
                node,
                &["name", "title", "text", "tooltip", "bg_color", "groups"],
            )?;
            for key in ["title", "text", "tooltip"] {
                if node.attr(key).is_some_and(|text| text.len() > MAX_LABEL) {
                    return Err(invalid(format!(
                        "Ribbon '{key}' exceeds the 4096-byte limit."
                    )));
                }
            }
            ribbon_text(node)?;
            ribbon_background(node)?;
            Ok("ribbon")
        }
        Some("notification_alert") => {
            attrs(node, &["name", "groups"])?;
            Ok("notification_alert")
        }
        _ => Err(invalid("Unregistered record-form display widget.")),
    }
}

fn structure<'a, N: Node<'a>>(
    node: N,
    parent: Option<N>,
    spec: &ModelSpec,
    key: &str,
    service: bool,
) -> ViewResult<()> {
    let kind = classify(node, parent, spec, key, service)?;
    if kind == "searchpanel" {
        return Ok(());
    } // category validation produces a typed unsupported descriptor
    if kind == "unwrap" {
        for child in node.children() {
            if child.tag() == "chatter" {
                return Err(layout_error(
                    "<chatter> cannot be placed inside a conditional wrapper.",
                ));
            }
            structure(child, parent, spec, key, service)?;
        }
        return Ok(());
    }
    if node.tag() == "footer"
        && node
            .children()
            .filter(|child| child.attr("special").is_some())
            .count()
            > 1
    {
        return Err(layout_error(
            "A <footer> allows at most one special button.",
        ));
    }
    if node.tag() == "form"
        && node
            .children()
            .filter(|child| child.tag() == "aside")
            .count()
            > 1
    {
        return Err(layout_error("A record form allows at most one <aside>."));
    }
    if node.tag() == "form"
        && node
            .children()
            .filter(|child| child.tag() == "chatter")
            .count()
            > 1
    {
        return Err(layout_error("A record form allows at most one <chatter>."));
    }
    for (position, child) in node.children().enumerate() {
        if node.tag() == "form" && child.tag() == "header" && position != 0 {
            return Err(layout_error(
                "<header> must be the first child of <form>, before the sheet and content.",
            ));
        }
        if node.tag() == "form"
            && child.tag() == "footer"
            && position + 1 != node.children().count()
        {
            return Err(layout_error(
                "<footer> must be the single last child of <form>, after the sheet and content.",
            ));
        }
        if node.tag() == "notebook" && child.tag() == "page" {
            let name = page_name(child)?;
            for prior in node.children().take(position) {
                if page_name(prior)? == name {
                    return Err(layout_error(format!(
                        "Duplicate notebook page name '{name}'."
                    )));
                }
            }
        }
        structure(child, Some(node), spec, key, service)?;
    }
    Ok(())
}

fn plan<'a, N: Node<'a>>(
    root: N,
    spec: &ModelSpec,
    key: &str,
    budget: &DescriptionBudget,
) -> ViewResult<Plan<N>> {
    let mut count_nodes = 0;
    count(root, 1, &mut count_nodes)?;
    let bytes = count_nodes
        .checked_mul(size_of::<Entry<N>>())
        .ok_or_else(|| invalid("View plan storage overflow."))?;
    let storage = budget.reserve(bytes)?;
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(count_nodes)
        .map_err(|_| invalid("View plan allocation failed."))?;
    let spare = if entries.capacity() > count_nodes {
        Some(
            budget.reserve(
                (entries.capacity() - count_nodes)
                    .checked_mul(size_of::<Entry<N>>())
                    .ok_or_else(|| invalid("View plan storage overflow."))?,
            )?,
        )
    } else {
        None
    };
    let service = key == "form"
        && match root.attr("record_mode") {
            None | Some("record") => false,
            Some("service") => true,
            Some(other) => return Err(invalid(format!("Unknown form record_mode '{other}'."))),
        };
    structure(root, None, spec, key, service)?;
    fn visit<'a, N: Node<'a>>(
        node: N,
        parent: Option<usize>,
        spec: &ModelSpec,
        key: &str,
        service: bool,
        entries: &mut Vec<Entry<N>>,
        budget: &DescriptionBudget,
    ) -> ViewResult<()> {
        let parent_node = parent.map(|index| entries[index].node);
        let kind = classify(node, parent_node, spec, key, service)?;
        if kind == "field" && node.attr("widget") == Some("code") {
            code_mode(node.attr("options"), budget)?;
        }
        if kind == "unwrap" {
            for child in node.children() {
                visit(child, parent, spec, key, service, entries, budget)?;
            }
            return Ok(());
        }
        if kind == "searchpanel" {
            return Ok(());
        } // the authorized host panel service attaches the panel after callback construction
        if kind != "field"
            && kind != "button"
            && kind != "stat_button"
            && kind != "cancel_button"
            && kind != "action_button"
            && boolean(node, "invisible", false)?
        {
            return Ok(());
        }
        if matches!(
            kind,
            "button" | "stat_button" | "cancel_button" | "action_button"
        ) && matches!(node.attr("invisible"), Some("1" | "True" | "true"))
        {
            return Ok(());
        }
        let previous = parent.and_then(|parent| entries[parent].last);
        let index = entries.len();
        entries.push(Entry {
            node,
            parent: parent.unwrap_or(0),
            kind,
            first: None,
            last: None,
            next: None,
        });
        if let Some(parent) = parent {
            if let Some(last) = entries[parent].last {
                entries[last].next = Some(index);
            } else {
                entries[parent].first = Some(index);
            }
            entries[parent].last = Some(index);
        }
        if node.tag() == "footer"
            && node
                .children()
                .filter(|child| child.attr("special").is_some())
                .count()
                > 1
        {
            return Err(layout_error(
                "A <footer> allows at most one special button.",
            ));
        }
        if node.tag() == "form"
            && node
                .children()
                .filter(|child| child.tag() == "aside")
                .count()
                > 1
        {
            return Err(layout_error("A record form allows at most one <aside>."));
        }
        for (position, child) in node.children().enumerate() {
            if node.tag() == "form" && child.tag() == "header" && position != 0 {
                return Err(layout_error(
                    "<header> must be the first child of <form>, before the sheet and content.",
                ));
            }
            if node.tag() == "form"
                && child.tag() == "footer"
                && position + 1 != node.children().count()
            {
                return Err(layout_error(
                    "<footer> must be the single last child of <form>, after the sheet and content.",
                ));
            }
            if node.tag() == "notebook" && child.tag() == "page" {
                let name = page_name(child)?;
                for prior in node.children().take(position) {
                    if page_name(prior)? == name {
                        return Err(layout_error(format!(
                            "Duplicate notebook page name '{name}'."
                        )));
                    }
                }
            }
            visit(child, Some(index), spec, key, service, entries, budget)?;
        }
        if kind == "search_group" && entries[index].first.is_none() {
            entries.pop();
            if let Some(parent) = parent {
                entries[parent].last = previous;
                if let Some(previous) = previous {
                    entries[previous].next = None;
                } else {
                    entries[parent].first = None;
                }
            }
        }
        Ok(())
    }
    visit(root, None, spec, key, service, &mut entries, budget)?;
    if service
        && entries
            .iter()
            .filter(|entry| entry.kind == "widget")
            .count()
            != 1
    {
        return Err(invalid(
            "A service form must contain exactly one registered widget.",
        ));
    }
    Ok(Plan {
        entries,
        _storage: storage,
        _spare: spare,
    })
}

pub(crate) fn validate(
    input: &CheckedViewInput<'_>,
    key: &str,
    budget: &DescriptionBudget,
) -> ViewResult<()> {
    let root = input
        .root()
        .ok_or_else(|| invalid("The view root is not readable."))?;
    plan(root, input.model_spec(), key, budget).map(drop)
}
pub(crate) fn build(
    input: &CheckedViewInput<'_>,
    key: &str,
    output: DescriptionValueBuilder<'_>,
) -> ViewResult<DescriptionValue> {
    let root = input
        .root()
        .ok_or_else(|| invalid("The view root is not readable."))?;
    lower_tree(root, input.model_spec(), key, output)
}
fn lower_tree<'a, N: Node<'a>>(
    root: N,
    spec: &'a ModelSpec,
    key: &str,
    output: DescriptionValueBuilder<'_>,
) -> ViewResult<DescriptionValue> {
    let plan = plan(root, spec, key, output.budget())?;
    output.object(|object| {
        object.set("root", |value| value.integer(0))?;
        object.set("nodes", |value| {
            value.array(plan.entries.len(), |index, value| {
                build_entry(&plan, index, spec, key, value)
            })
        })
    })
}
fn set_children<'a, N: Node<'a>>(
    object: &mut erp_sdk::views::DescriptionObjectBuilder<'_>,
    plan: &Plan<N>,
    index: usize,
) -> ViewResult<()> {
    let mut count = 0;
    let mut next = plan.entries[index].first;
    while let Some(child) = next {
        count += 1;
        next = plan.entries[child].next;
    }
    next = plan.entries[index].first;
    object.set("children", |value| {
        value.array(count, |_, value| {
            let child = next.ok_or_else(|| invalid("View plan child linkage changed."))?;
            next = plan.entries[child].next;
            value.integer(i64::try_from(child).map_err(|_| invalid("View node index overflow."))?)
        })
    })
}

#[derive(Clone, Copy)]
enum PanelIssue<'a> {
    Static(&'static str),
    PanelAttribute(&'a str),
    CategoryAttribute(&'a str),
    DynamicBoolean(&'static str),
}
impl PanelIssue<'_> {
    fn parts(&self) -> [&str; 3] {
        match self {
            Self::Static(text) => [text, "", ""],
            Self::PanelAttribute(name) => ["Search panel attribute '", name, "' is not supported."],
            Self::CategoryAttribute(name) => [
                "Search panel category attribute '",
                name,
                "' is not supported.",
            ],
            Self::DynamicBoolean(key) => [
                "Dynamic '",
                key,
                "' modifier is unsupported by the current human UI contract.",
            ],
        }
    }
    fn build(&self, output: DescriptionValueBuilder<'_>) -> ViewResult<DescriptionValue> {
        const SUFFIX: &str = " Supported search-panel extensions require a later release.";
        let parts = self.parts();
        let bytes = parts.iter().try_fold(SUFFIX.len(), |sum, part| {
            sum.checked_add(part.len())
                .ok_or_else(|| invalid("Search panel message storage overflow."))
        })?;
        let _storage = output.budget().reserve(bytes)?;
        let mut text = String::new();
        text.try_reserve_exact(bytes)
            .map_err(|_| invalid("Search panel message allocation failed."))?;
        let _spare = if text.capacity() > bytes {
            Some(output.budget().reserve(text.capacity() - bytes)?)
        } else {
            None
        };
        for part in parts {
            text.push_str(part);
        }
        text.push_str(SUFFIX);
        output.text(&text)
    }
}
enum Panel<'a, N> {
    Selection { node: N, field: &'a FieldSpec },
    Unsupported(PanelIssue<'a>),
}
fn panel<'a, 's, N: Node<'a>>(root: N, spec: &'s ModelSpec) -> ViewResult<Option<Panel<'a, N>>>
where
    's: 'a,
{
    fn find<'a, N: Node<'a>>(
        node: N,
        inherited: Option<PanelIssue<'a>>,
        first: &mut Option<N>,
        total: &mut usize,
        issue: &mut Option<PanelIssue<'a>>,
    ) {
        if issue.is_some() || matches!(node.attr("invisible"), Some("1" | "true" | "True")) {
            return;
        }
        let inherited = if boolean(node, "invisible", false).is_err() {
            Some(PanelIssue::DynamicBoolean("invisible"))
        } else {
            inherited
        };
        if node.tag() == "searchpanel" {
            if let Some(inherited) = inherited {
                *issue = Some(inherited);
                return;
            }
            *total += 1;
            if first.is_none() {
                *first = Some(node);
            }
            return;
        }
        for child in node.children() {
            find(child, inherited, first, total, issue);
        }
    }
    let mut first = None;
    let mut total = 0;
    let mut issue = None;
    find(root, None, &mut first, &mut total, &mut issue);
    if let Some(issue) = issue {
        return Ok(Some(Panel::Unsupported(issue)));
    }
    if total == 0 {
        return Ok(None);
    }
    if total != 1 {
        return Ok(Some(Panel::Unsupported(PanelIssue::Static(
            "Only one search panel is supported.",
        ))));
    }
    let panel = first.ok_or_else(|| invalid("Search panel traversal lost its only category."))?;
    if panel
        .attr("view_types")
        .is_some_and(|types| !types.split(',').any(|kind| kind.trim() == "list"))
    {
        return Ok(None);
    }
    if let Some((name, _)) = panel
        .attributes()
        .find(|(name, _)| !matches!(*name, "view_types" | "invisible"))
    {
        return Ok(Some(Panel::Unsupported(PanelIssue::PanelAttribute(name))));
    }
    if boolean(panel, "invisible", false).is_err() {
        return Ok(Some(Panel::Unsupported(PanelIssue::DynamicBoolean(
            "invisible",
        ))));
    }
    let mut fields = panel
        .children()
        .filter(|node| !matches!(node.attr("invisible"), Some("1" | "true" | "True")));
    let Some(node) = fields.next() else {
        return Ok(None);
    };
    if node.tag() != "field" || fields.next().is_some() {
        return Ok(Some(Panel::Unsupported(PanelIssue::Static(
            "Only one single-select selection category is supported.",
        ))));
    }
    if let Some((name, _)) = node.attributes().find(|(name, _)| {
        !matches!(
            *name,
            "name"
                | "string"
                | "select"
                | "enable_counters"
                | "expand"
                | "limit"
                | "invisible"
                | "hierarchize"
        )
    }) {
        return Ok(Some(Panel::Unsupported(PanelIssue::CategoryAttribute(
            name,
        ))));
    }
    if node.attr("select").is_some_and(|value| value != "one") {
        return Ok(Some(Panel::Unsupported(PanelIssue::Static(
            "Multi-select search panel filters are not supported.",
        ))));
    }
    let name = node
        .attr("name")
        .ok_or_else(|| invalid("Search panel field requires name."))?;
    let field = spec.field(name).ok_or_else(|| {
        invalid(format!(
            "Unknown search panel field '{}.{name}'.",
            spec.name
        ))
    })?;
    if !field.store {
        return Ok(Some(Panel::Unsupported(PanelIssue::Static(
            "Search panel selection category requires a stored selection field.",
        ))));
    }
    let FieldType::Selection {
        values: Some(choices),
        ..
    } = &field.ty
    else {
        return Ok(Some(Panel::Unsupported(PanelIssue::Static(
            "Search panel selection category requires a static selection field.",
        ))));
    };
    if choices.is_empty() || choices.len() > 32 {
        return Ok(Some(Panel::Unsupported(PanelIssue::Static(
            "Search panel selection category requires 1–32 selection choices.",
        ))));
    }
    for (index, (key, label)) in choices.iter().enumerate() {
        if key.trim().is_empty()
            || label.trim().is_empty()
            || key.len() > 256
            || label.len() > 256
            || choices.iter().take(index).any(|(prior, _)| prior == key)
        {
            return Ok(Some(Panel::Unsupported(PanelIssue::Static(
                "Search panel selection category choices require distinct nonempty keys and nonempty labels of at most 256 bytes.",
            ))));
        }
    }
    for (key, default) in [
        ("invisible", false),
        ("enable_counters", false),
        ("expand", false),
        ("hierarchize", true),
    ] {
        if boolean(node, key, default).is_err() {
            return Ok(Some(Panel::Unsupported(PanelIssue::DynamicBoolean(key))));
        }
    }
    if boolean(node, "invisible", false)? {
        return Ok(None);
    }
    if let Some(limit) = node.attr("limit") {
        limit
            .trim()
            .parse::<u64>()
            .map_err(|_| invalid("Search panel limit must be a nonnegative integer."))?;
    }
    let label = node.attr("string").unwrap_or(&field.string);
    if label.trim().is_empty() || label.len() > 256 {
        return Err(invalid("Search panel label requires 1–256 bytes."));
    }
    Ok(Some(Panel::Selection { node, field }))
}
fn set_panel<'a, N: Node<'a>>(
    object: &mut erp_sdk::views::DescriptionObjectBuilder<'_>,
    root: N,
    spec: &'a ModelSpec,
    budget: &DescriptionBudget,
) -> ViewResult<()> {
    if sections::set(object, root, spec, budget)? {
        return Ok(());
    }
    let Some(panel) = panel(root, spec)? else {
        return Ok(());
    };
    object.set("search_panel", |output| match panel {
        Panel::Unsupported(issue) => output.variant("unsupported", |value| {
            value.object(|object| object.set("message", |value| issue.build(value)))
        }),
        Panel::Selection { node, field } => output.variant("selection", |value| {
            value.object(|object| {
                object.set("field", |value| value.field_handle(spec.name, field.name))?;
                object.set("label", |value| {
                    value.text(node.attr("string").unwrap_or(&field.string))
                })?;
                object.set("enable_counters", |value| {
                    value.boolean(boolean(node, "enable_counters", false)?)
                })?;
                object.set("expand", |value| {
                    value.boolean(boolean(node, "expand", false)?)
                })?;
                Ok(())
            })
        }),
    })
}
/// Only the selection panel of a checked search view, without validating unrelated facet syntax.
/// The host applies the authorized contextual default and selection notices after this payload.
pub(super) fn build_panel_description(
    input: &CheckedViewInput<'_>,
    budget: &DescriptionBudget,
    schema: &erp_sdk::views::ViewSchema,
) -> ViewResult<erp_sdk::views::DescriptionPayload> {
    let root = input
        .root()
        .ok_or_else(|| invalid("The search view root is not readable."))?;
    if root.tag() != "search" {
        return Err(invalid("Selected search view must have a <search> root."));
    }
    let mut nodes = 0;
    count(root, 1, &mut nodes)?;
    erp_sdk::views::DescriptionBuilder::new(schema, budget).build(|output| {
        output.object(|object| {
            object.set("root", |value| value.integer(0))?;
            object.set("nodes", |value| {
                value.array(1, |_, value| {
                    value.variant("search", |value| {
                        value.object(|object| {
                            object.set("children", |value| {
                                value.array(0, |_, _| {
                                    Err(invalid("Empty search panel children changed."))
                                })
                            })?;
                            set_panel(object, root, input.model_spec(), budget)
                        })
                    })
                })
            })
        })
    })
}

fn set_view_actions<'a, N: Node<'a>>(
    object: &mut erp_sdk::views::DescriptionObjectBuilder<'_>,
    node: N,
) -> ViewResult<()> {
    for key in ["create", "edit", "delete"] {
        if node.attr(key).is_some() {
            let val = boolean(node, key, true)?;
            object.set(key, |value| value.boolean(val))?;
        }
    }
    Ok(())
}

fn build_entry<'a, N: Node<'a>>(
    plan: &Plan<N>,
    index: usize,
    spec: &'a ModelSpec,
    _key: &str,
    output: DescriptionValueBuilder<'_>,
) -> ViewResult<DescriptionValue> {
    let entry = &plan.entries[index];
    let node = entry.node;
    let budget = output.budget();
    let label = node.attr("string").unwrap_or(""); // absent captions intentionally remain empty (existing wire contract)
    output.variant(entry.kind, |value| {
        value.object(|object| {
            match entry.kind {
                "form" => {
                    object.set("record_mode", |value| {
                        value.text(node.attr("record_mode").unwrap_or("record"))
                    })?;
                    set_children(object, plan, index)?;
                    set_view_actions(object, node)?;
                }
                "list" => {
                    set_children(object, plan, index)?;
                    if let Some(raw) = node.attr("limit") {
                        let limit = raw.trim().parse::<i64>().map_err(|_| {
                            invalid("XML list limit must be a nonnegative integer.")
                        })?;
                        if !(0..=9_007_199_254_740_991).contains(&limit) {
                            return Err(invalid(
                                "List page limit must be a positive browser-safe integer.",
                            ));
                        }
                        if limit != 0 {
                            object.set("page_limit", |value| value.integer(limit))?;
                        }
                    }
                    set_view_actions(object, node)?;
                }
                "search" => {
                    set_children(object, plan, index)?;
                    set_panel(object, node, spec, budget)?;
                }
                "sheet" | "title" | "aside" | "header" | "footer" | "button_box" | "notebook" => {
                    set_children(object, plan, index)?
                }
                "heading" => {
                    object.set("level", |value| {
                        value.integer(if node.tag() == "h1" { 1 } else { 2 })
                    })?;
                    set_children(object, plan, index)?;
                }
                "group" => {
                    object.set("label", |value| value.text(label))?;
                    object.set("columns", |value| {
                        value.integer(i64::from(tracks(
                            node,
                            "col",
                            if node.children().any(|child| child.tag() == "group") {
                                4
                            } else {
                                2
                            },
                        )?))
                    })?;
                    object.set("colspan", |value| {
                        value.integer(i64::from(tracks(node, "colspan", 2)?))
                    })?;
                    set_children(object, plan, index)?;
                }
                "page" => {
                    let name = page_name(node)?;
                    object.set("name", |value| value.text(name))?;
                    object.set("label", |value| {
                        value.text(if label.trim().is_empty() { name } else { label })
                    })?;
                    set_children(object, plan, index)?;
                }
                "separator" => object.set("label", |value| value.text(label))?,
                "field" => {
                    let field = field(spec, node)?;
                    let parent = Some(
                        plan.entries
                            .get(entry.parent)
                            .ok_or_else(|| invalid("View plan parent linkage changed."))?
                            .node,
                    );
                    let widget = widget(spec, node, parent)?;
                    object.set("field", |value| value.field_handle(spec.name, field.name))?;
                    object.set("label", |value| {
                        value.text(node.attr("string").unwrap_or(&field.string))
                    })?;
                    object.set("widget", |value| value.text(widget))?;
                    object.set("force_save", |value| {
                        value.boolean(boolean(node, "force_save", false)?)
                    })?;
                    object.set("options", |value| {
                        if widget == "code" {
                            // Code fields keep their closed mode admission; every other
                            // widget goes through the typed presentation options.
                            let mode = code_mode(node.attr("options"), value.budget())?;
                            value.object(|object| {
                                object.set("mode", |value| value.text(mode.as_str()))?;
                                if let Some(visible) = node.attr("statusbar_visible") {
                                    object.set("visible", |value| {
                                        value.array(visible.split(',').count(), |index, value| {
                                            value.text(
                                                visible
                                                    .split(',')
                                                    .nth(index)
                                                    .ok_or_else(|| {
                                                        invalid("Statusbar key index changed.")
                                                    })?
                                                    .trim(),
                                            )
                                        })
                                    })?;
                                }
                                Ok(())
                            })
                        } else {
                            set_field_options(spec, node, widget, value)
                        }
                    })?;
                    set_modifiers(
                        plan,
                        object,
                        node,
                        parent.is_some_and(|parent| parent.tag() == "button"),
                    )?;
                }
                "button" | "stat_button" | "cancel_button" | "action_button" => {
                    object.set("label", |value| value.text(button_label(node)?))?;
                    object.set("variant", |value| value.text(button_variant(node)?))?;
                    if entry.kind == "action_button" {
                        object.set("target", |value| {
                            value.variant(
                                match node.attr("type") {
                                    Some("action") => "action",
                                    Some("url") => "url",
                                    None => "inert",
                                    _ => {
                                        return Err(invalid(
                                            "Invalid admitted action button target.",
                                        ));
                                    }
                                },
                                |value| {
                                    value.object(|target| {
                                        match node.attr("type") {
                                            Some("action") => target.set("id", |value| {
                                                value.integer(button_action_id(node)?)
                                            })?,
                                            Some("url") => target.set("href", |value| {
                                                value.text(node.attr("href").ok_or_else(|| {
                                                    invalid("URL button requires href.")
                                                })?)
                                            })?,
                                            None => {}
                                            _ => {
                                                return Err(invalid(
                                                    "Invalid admitted action button target.",
                                                ));
                                            }
                                        };
                                        Ok(())
                                    })
                                },
                            )
                        })?;
                        if let Some(confirm) = node.attr("confirm") {
                            object.set("confirm", |value| value.text(confirm))?;
                        }
                    } else if entry.kind != "cancel_button" {
                        object.set("name", |value| {
                            value.text(
                                node.attr("name")
                                    .ok_or_else(|| invalid("Button requires a name."))?,
                            )
                        })?;
                        object.set("button_type", |value| value.text("object"))?;
                        if let Some(confirm) = node.attr("confirm") {
                            object.set("confirm", |value| value.text(confirm))?;
                        }
                    }
                    set_button_details(object, node)?;
                    set_modifiers(plan, object, node, false)?;
                    if entry.kind == "stat_button" {
                        set_children(object, plan, index)?;
                    }
                }
                "ribbon" => {
                    object.set("text", |value| value.text(ribbon_text(node)?))?;
                    object.set("background", |value| value.text(ribbon_background(node)?))?;
                    if let Some(tooltip) = node.attr("tooltip") {
                        object.set("tooltip", |value| value.text(tooltip))?;
                    }
                }
                "notification_alert" => {}
                "chatter" => {
                    for flag in CHATTER_FLAGS {
                        object.set(flag, |value| value.boolean(chatter_flag(node, flag)?))?;
                    }
                    object.set("has_activities", |value| {
                        value.boolean(spec.inherits("mail.activity.mixin"))
                    })?;
                }
                "widget" => {
                    let widget = node
                        .attr("name")
                        .ok_or_else(|| invalid("Service widget requires a name."))?;
                    object.set("widget", |value| value.text(widget))?;
                    object.set("options", |value| {
                        value.object(|object| {
                            if let Some(scopes) = node.attr("scope_kinds") {
                                object.set("scope_kinds", |value| {
                                    value.array(scopes.split(',').count(), |index, value| {
                                        value.text(scopes.split(',').nth(index).ok_or_else(
                                            || invalid("Service widget scope index changed."),
                                        )?)
                                    })
                                })?;
                            }
                            Ok(())
                        })
                    })?;
                    object.set("modifiers", |value| value.object(|_| Ok(())))?;
                }
                "search_field" => {
                    let field = field(spec, node)?;
                    object.set("field", |value| value.field_handle(spec.name, field.name))?;
                    object.set("label", |value| {
                        value.text(node.attr("string").unwrap_or(&field.string))
                    })?;
                    if let Some(source) = node.attr("filter_domain").filter(|text| !text.is_empty())
                    {
                        object.set("filter_domain", |value| {
                            source_expression(source, value, |_| Ok(()))
                        })?;
                    }
                    if let Some(operator) = node.attr("operator") {
                        object.set("operator", |value| value.text(operator))?;
                    }
                }
                "search_filter" => {
                    object.set("name", |value| value.text(node.attr("name").unwrap_or("")))?;
                    object.set("label", |value| value.text(label))?;
                    for key in ["domain", "context"] {
                        if let Some(source) = node.attr(key).filter(|text| !text.is_empty()) {
                            object
                                .set(key, |value| source_expression(source, value, |_| Ok(())))?;
                        }
                    }
                }
                "search_group" => {
                    object.set("label", |value| value.text(label))?;
                    object.set("expand", |value| {
                        value.boolean(boolean(node, "expand", false)?)
                    })?;
                    set_children(object, plan, index)?;
                }
                "search_separator" => {}
                _ => return Err(invalid("Unknown admitted builtin node kind.")),
            }
            Ok(())
        })
    })
}
fn set_field_options<'a, N: Node<'a>>(
    spec: &ModelSpec,
    node: N,
    widget: &str,
    output: DescriptionValueBuilder<'_>,
) -> ViewResult<DescriptionValue> {
    use erp_sdk::expr::{Expr, Literal};
    let source = node.attr("options").unwrap_or("{}"); // absent options are empty
    let limits = erp_sdk::expr::EvalLimits::default();
    let bound = erp_sdk::expr::source_preflight(source, limits)
        .map_err(|error| invalid(format!("Invalid field options: {error}")))?;
    let _scratch = output.budget().reserve(bound.reserve_bytes())?;
    let parsed = erp_sdk::expr::parse_with_limits(source, limits)
        .map_err(|error| invalid(format!("Invalid field options: {error}")))?;
    let Expr::Dict(entries) = &parsed else {
        return Err(invalid("Field options must be a literal dictionary."));
    };
    output.object(|object| {
        for (key, expr) in entries {
            let Expr::Literal(Literal::Str(key)) = key else { return Err(invalid("Field option keys must be literal text.")); };
            object.set(key, |value| field_option_value(spec, field(spec, node)?, widget, key, expr, value))?;
        }
        for key in ["help", "placeholder", "autocomplete"] {
            let value = if key == "help" { node.attr(key).or(if widget == "statusbar" { None } else { field(spec, node)?.help }) } else { node.attr(key) };
            if let Some(value) = value {
                if erp_sdk::views::wire::field_option_kind(widget, key).is_none() {
                    return Err(invalid(format!("Field option '{key}' is not supported by widget '{widget}'.")));
                }
                object.set(key, |slot| slot.text(value))?;
            }
        }
        if node.attr("password").is_some() {
            if !matches!(widget, "char" | "text" | "password") { return Err(invalid("password requires a text widget.")); }
            object.set("password", |value| value.boolean(boolean(node, "password", false)?))?;
        }
        if node.attr("nolabel").is_some() {
            if erp_sdk::views::wire::field_option_kind(widget, "nolabel").is_none() {
                return Err(invalid(format!("Field option 'nolabel' is not supported by widget '{widget}'.")));
            }
            object.set("nolabel", |value| value.boolean(boolean(node, "nolabel", false)?))?;
        }
        for key in ["domain", "context"] {
            if let Some(source) = node.attr(key) {
                if erp_sdk::views::wire::field_option_kind(widget, key).is_none() {
                    return Err(invalid(format!("Field option '{key}' is not supported by widget '{widget}'.")));
                }
                object.set(key, |value| source_expression(source, value, |_| Ok(())))?;
            }
        }
        if let Some(visible) = node.attr("statusbar_visible") {
            object.set("visible", |value| value.array(visible.split(',').count(), |index, value| {
                value.text(visible.split(',').nth(index).ok_or_else(|| invalid("Statusbar key index changed."))?.trim())
            }))?;
        }
        if widget == "monetary" && !entries.iter().any(|(key, _)| matches!(key, Expr::Literal(Literal::Str(key)) if key == "currency_field"))
            && let FieldType::Monetary { currency_field: Some(currency_field), .. } = &field(spec, node)?.ty
        {
            object.set("currency_field", |value| value.field_handle(spec.name, currency_field))?;
        }
        Ok(())
    })
}
fn field_option_value(
    spec: &ModelSpec,
    occurrence: &FieldSpec,
    widget: &str,
    key: &str,
    expr: &erp_sdk::expr::Expr,
    output: DescriptionValueBuilder<'_>,
) -> ViewResult<DescriptionValue> {
    use erp_sdk::expr::{Expr, Literal};
    use erp_sdk::views::wire::{FieldOptionKind as K, field_option_kind};
    let kind = field_option_kind(widget, key).ok_or_else(|| {
        invalid(format!(
            "Field option '{key}' is not supported by widget '{widget}'."
        ))
    })?;
    let wrong = || {
        invalid(format!(
            "Invalid value for field option '{key}' on widget '{widget}'."
        ))
    };
    match (kind, expr) {
        (K::Text, Expr::Literal(Literal::Str(value))) => output.text(value),
        (K::Bool, Expr::Literal(Literal::Bool(value))) => output.boolean(*value),
        (K::Integer, Expr::Literal(Literal::Int(value))) if (0..=4096).contains(value) => {
            output.integer(*value)
        }
        (K::Field, Expr::Literal(Literal::Str(value))) => {
            if key == "whitelist_fname" {
                return Err(invalid(
                    "Selection whitelist field data requires a later release.",
                ));
            }
            if key == "color_field"
                && matches!(
                    widget,
                    "many2many" | "many2many_tags" | "many2many_tags_color_dot"
                )
            {
                let comodel = match &occurrence.ty {
                    FieldType::Many2many { comodel, .. } | FieldType::One2many { comodel, .. } => {
                        comodel.as_str()
                    }
                    _ => return Err(wrong()),
                };
                return output.field_handle(comodel, value);
            }
            let field = spec.field(value).ok_or_else(wrong)?;
            if key == "placeholder_field" && !matches!(field.ty.type_name(), "char" | "text") {
                return Err(wrong());
            }
            if key == "currency_field"
                && !matches!(&field.ty, FieldType::Many2one { comodel, .. } if comodel == "res.currency")
            {
                return Err(wrong());
            }
            if matches!(key, "start_date_field" | "end_date_field")
                && field.ty.type_name() != occurrence.ty.type_name()
            {
                return Err(wrong());
            }
            output.field_handle(spec.name, field.name)
        }
        (K::Strings, Expr::List(values) | Expr::Tuple(values)) => {
            output.array(values.len(), |index, output| match &values[index] {
                Expr::Literal(Literal::Str(value)) => output.text(value),
                _ => Err(wrong()),
            })
        }
        (K::Digits, Expr::List(values) | Expr::Tuple(values)) if values.len() == 2 => {
            output.array(2, |index, output| match &values[index] {
                Expr::Literal(Literal::Int(value)) if (0..=20).contains(value) => {
                    output.integer(*value)
                }
                _ => Err(wrong()),
            })
        }
        (K::Expression, _) => {
            let admitted =
                erp_sdk::expr::wire::ValidatedAst::from_expr(expr).map_err(|_| wrong())?;
            output.expression(&admitted)
        }
        _ => Err(wrong()),
    }
}

fn set_button_details<'a>(
    object: &mut erp_sdk::views::DescriptionObjectBuilder<'_>,
    node: impl Node<'a>,
) -> ViewResult<()> {
    let declared = [
        "size",
        "icon",
        "title",
        "help",
        "data-hotkey",
        "debounce",
        "block-ui",
        "confirm-title",
        "confirm-label",
        "cancel-label",
        "context",
    ]
    .iter()
    .any(|key| node.attr(key).is_some())
        || button_tokens(node).any(|token| token == "rounded-circle" || !presentation_class(token));
    if !declared {
        return Ok(());
    }
    object.set("details", |value| {
        value.object(|details| {
            details.set("size", |value| value.text(button_size(node)?))?;
            details.set("circle", |value| {
                value.boolean(has_button_class(node, "rounded-circle"))
            })?;
            details.set("classes", |value| {
                value.array(
                    button_tokens(node)
                        .filter(|token| !presentation_class(token))
                        .count(),
                    |index, value| {
                        value.text(
                            button_tokens(node)
                                .filter(|token| !presentation_class(token))
                                .nth(index)
                                .ok_or_else(|| invalid("Button class index changed."))?,
                        )
                    },
                )
            })?;
            details.set("debounce_ms", |value| value.integer(button_debounce(node)?))?;
            details.set("block_ui", |value| {
                value.boolean(boolean(node, "block-ui", false)?)
            })?;
            for (xml, wire) in [
                ("icon", "icon"),
                ("title", "title"),
                ("help", "help"),
                ("data-hotkey", "hotkey"),
                ("confirm-title", "confirm_title"),
                ("confirm-label", "confirm_label"),
                ("cancel-label", "cancel_label"),
            ] {
                if let Some(text) = node.attr(xml) {
                    details.set(wire, |value| value.text(text))?;
                }
            }
            if let Some(source) = node.attr("context") {
                details.set("context", |value| {
                    source_expression(source, value, |_| Ok(()))
                })?;
            }
            Ok(())
        })
    })
}
fn set_modifiers<'a, N: Node<'a>>(
    plan: &Plan<N>,
    object: &mut erp_sdk::views::DescriptionObjectBuilder<'_>,
    node: N,
    statistic: bool,
) -> ViewResult<()> {
    object.set("modifiers", |value| {
        value.object(|object| {
            for key in ["readonly", "required", "invisible"] {
                let source = if statistic && key == "readonly" {
                    Some("True")
                } else {
                    node.attr(key)
                };
                if let Some(source) = source {
                    // Bool spellings are modifier syntax only. A search domain containing `1`
                    // remains an integer expression, rather than silently becoming a boolean.
                    let canonical = match source {
                        "1" | "true" | "True" => "True",
                        "0" | "false" | "False" => "False",
                        _ => source,
                    };
                    object.set(key, |value| {
                        source_expression(canonical, value, |expr| {
                            if node.tag() == "field" || node.tag() == "button" && key == "invisible"
                            {
                                button_expression(
                                    expr,
                                    node.attr("name").unwrap_or("cancel"),
                                    plan,
                                )?;
                            }
                            Ok(())
                        })
                    })?;
                }
            }
            Ok(())
        })
    })
}

// Walk the already bounded borrowed AST. No separate name set or cloned expression is needed.
fn button_expression<'a, N: Node<'a>>(
    expr: &erp_sdk::expr::Expr,
    method: &str,
    plan: &Plan<N>,
) -> ViewResult<()> {
    use erp_sdk::expr::Expr;
    match expr {
        Expr::Literal(_) => {}
        Expr::Name(name) => {
            if !plan.entries.iter().any(|entry| {
                entry.kind == "field" && entry.node.attr("name") == Some(name.as_str())
            }) {
                return Err(layout_error(format!(
                    "Button '{method}' invisible expression uses '{name}', which is not a field shown by this view."
                )));
            }
        }
        Expr::Unary { operand, .. } => button_expression(operand, method, plan)?,
        Expr::Binary { left, right, .. } => {
            button_expression(left, method, plan)?;
            button_expression(right, method, plan)?;
        }
        Expr::BoolOp { values, .. } | Expr::List(values) | Expr::Tuple(values) => {
            for value in values {
                button_expression(value, method, plan)?;
            }
        }
        Expr::Compare {
            left, comparators, ..
        } => {
            button_expression(left, method, plan)?;
            for value in comparators {
                button_expression(value, method, plan)?;
            }
        }
        Expr::IfExp { test, body, orelse } => {
            for part in [test, body, orelse] {
                button_expression(part, method, plan)?;
            }
        }
        _ => {
            return Err(layout_error(
                "Button invisible expressions support names, literals, comparisons, not/and/or, arithmetic and conditionals only.",
            ));
        }
    }
    Ok(())
}

fn source_expression(
    source: &str,
    output: DescriptionValueBuilder<'_>,
    validate: impl FnOnce(&erp_sdk::expr::Expr) -> ViewResult<()>,
) -> ViewResult<DescriptionValue> {
    let limits = erp_sdk::expr::EvalLimits::default();
    let bound = erp_sdk::expr::source_preflight(source, limits)
        .map_err(|error| invalid(format!("Unsupported view expression: {error}")))?;
    // Preflight is allocation-free. Admission must succeed before parser or wire AST scratch
    // exists, and remain alive while both parsed and admitted trees coexist with builder copy.
    let _scratch = output.budget().reserve(bound.reserve_bytes())?;
    let parsed = erp_sdk::expr::parse_with_limits(source, limits)
        .map_err(|error| invalid(format!("Unsupported view expression: {error}")))?;
    validate(&parsed)?;
    let admitted = erp_sdk::expr::wire::ValidatedAst::from_expr(&parsed)
        .map_err(|error| invalid(format!("Unsupported view expression: {error}")))?;
    drop(parsed);
    let result = output.expression(&admitted);
    drop(admitted);
    result
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test fixture assertions"
)]
mod tests {
    use erp_sdk::views::{
        DescriptionBudget, DescriptionBuilder, DescriptionEncoder, ViewCatalogue, ViewDescriptor,
        ViewError, ViewLimits, form_schema, list_schema, search_schema,
    };
    use erp_sdk::{
        __private::{FieldDecl, ModelDecl},
        RegistryBuilder,
    };
    use serde_json::Value;

    static MODEL: ModelDecl = ModelDecl::new("base", "h18.fixture").fields(&[
        FieldDecl::char("name"),
        FieldDecl::text("source"),
        FieldDecl::integer("count"),
        FieldDecl::boolean("active"),
        FieldDecl::selection("state", &[("draft", "Draft"), ("done", "Done")]),
        FieldDecl::selection("stage", &[("new", "New"), ("closed", "Closed")]),
        FieldDecl::many2one("partner_id", "res.partner"),
        FieldDecl::many2one("parent_id", "h18.fixture"),
        FieldDecl::one2many("child_ids", "h18.fixture", "parent_id"),
    ]);

    // Fixture-only mixins: no registered model inherits mail.thread until.
    static THREAD: ModelDecl =
        ModelDecl::new("base", "mail.thread").kind(erp_sdk::ModelKind::Abstract);
    static ACTIVITY: ModelDecl =
        ModelDecl::new("base", "mail.activity.mixin").kind(erp_sdk::ModelKind::Abstract);
    static THREADED: ModelDecl = ModelDecl::new("base", "h18.threaded")
        .inherit(&["mail.thread"])
        .fields(&[FieldDecl::char("name")]);
    static SCHEDULED: ModelDecl = ModelDecl::new("base", "h18.scheduled")
        .inherit(&["mail.thread", "mail.activity.mixin"])
        .fields(&[FieldDecl::char("name")]);

    fn fixture(xml: &str, kind: &str) -> Result<Value, ViewError> {
        fixture_budget(xml, kind, &DescriptionBudget::new())
    }

    fn fixture_budget(
        xml: &str,
        kind: &str,
        budget: &DescriptionBudget,
    ) -> Result<Value, ViewError> {
        fixture_model("h18.fixture", xml, kind, budget)
    }

    fn fixture_model(
        model: &str,
        xml: &str,
        kind: &str,
        budget: &DescriptionBudget,
    ) -> Result<Value, ViewError> {
        let registry = RegistryBuilder::new([&erp_addon_base::ADDON, &crate::ADDON])
            .with_decls([&MODEL, &THREAD, &ACTIVITY, &THREADED, &SCHEDULED])
            .build()
            .unwrap();
        let spec = registry.model(model).unwrap();
        let catalogue =
            ViewCatalogue::new(vec![ViewDescriptor::new(1, "fixture", spec.name, xml)])?;
        let combined = catalogue.combine(1)?;
        let schema = match kind {
            "form" => form_schema()?,
            "list" => list_schema()?,
            "search" => search_schema()?,
            _ => panic!("test kind"),
        };
        let payload = DescriptionBuilder::new(&schema, budget)
            .build(|root| super::lower_tree(combined.arch().root(), spec, kind, root))?;
        let bytes = DescriptionEncoder::new(budget, &ViewLimits::default()).encode(&payload)?;
        Ok(serde_json::from_slice(bytes.as_json_bytes()).unwrap())
    }

    #[test]
    fn field_widget_nonedit_placeholder_is_rejected() {
        let error = fixture(
            r#"<form><field name="active" widget="boolean" placeholder="Unused"/></form>"#,
            "form",
        )
        .unwrap_err();
        match error {
            ViewError::InvalidDescription(message) => assert_eq!(
                message,
                "Field option 'placeholder' is not supported by widget 'boolean'."
            ),
            other => panic!("Unexpected error kind: {other:?}"),
        }
    }

    #[test]
    fn priority_field_admission() {
        let value = fixture(
            r#"<form><field name="state" widget="priority"/></form>"#,
            "form",
        )
        .unwrap();
        assert_eq!(value["nodes"][1]["widget"], "priority");

        let error = fixture(
            r#"<form><field name="name" widget="priority"/></form>"#,
            "form",
        )
        .unwrap_err();
        match error {
            ViewError::InvalidDescription(message) => {
                assert_eq!(
                    message,
                    "Field widget 'priority' does not support type 'char'."
                );
            }
            other => panic!("Unexpected error kind: {other:?}"),
        }
    }

    #[test]
    fn color_widgets_admission() {
        // color widget admitted on char field
        let value = fixture(
            r#"<form><field name="name" widget="color"/></form>"#,
            "form",
        )
        .unwrap();
        assert_eq!(value["nodes"][1]["widget"], "color");

        // color widget refused on integer field
        let error = fixture(
            r#"<form><field name="count" widget="color"/></form>"#,
            "form",
        )
        .unwrap_err();
        match error {
            ViewError::InvalidDescription(message) => {
                assert_eq!(
                    message,
                    "Field widget 'color' does not support type 'integer'."
                );
            }
            other => panic!("Unexpected error kind: {other:?}"),
        }

        // color_picker widget admitted on integer field
        let value = fixture(
            r#"<form><field name="count" widget="color_picker"/></form>"#,
            "form",
        )
        .unwrap();
        assert_eq!(value["nodes"][1]["widget"], "color_picker");

        // color_picker widget refused on char field
        let error = fixture(
            r#"<form><field name="name" widget="color_picker"/></form>"#,
            "form",
        )
        .unwrap_err();
        match error {
            ViewError::InvalidDescription(message) => {
                assert_eq!(
                    message,
                    "Field widget 'color_picker' does not support type 'char'."
                );
            }
            other => panic!("Unexpected error kind: {other:?}"),
        }
    }

    #[test]
    fn copy_clipboard_widgets_admission() {
        // CopyClipboardChar admitted on char field
        let value = fixture(
            r#"<form><field name="name" widget="CopyClipboardChar"/></form>"#,
            "form",
        )
        .unwrap();
        assert_eq!(value["nodes"][1]["widget"], "CopyClipboardChar");

        // CopyClipboardChar refused on integer field
        let error = fixture(
            r#"<form><field name="count" widget="CopyClipboardChar"/></form>"#,
            "form",
        )
        .unwrap_err();
        match error {
            ViewError::InvalidDescription(message) => {
                assert_eq!(
                    message,
                    "Field widget 'CopyClipboardChar' does not support type 'integer'."
                );
            }
            other => panic!("Unexpected error kind: {other:?}"),
        }

        // CopyClipboardText admitted on text field
        let value = fixture(
            r#"<form><field name="source" widget="CopyClipboardText"/></form>"#,
            "form",
        )
        .unwrap();
        assert_eq!(value["nodes"][1]["widget"], "CopyClipboardText");

        // CopyClipboardText refused on integer field
        let error = fixture(
            r#"<form><field name="count" widget="CopyClipboardText"/></form>"#,
            "form",
        )
        .unwrap_err();
        match error {
            ViewError::InvalidDescription(message) => {
                assert_eq!(
                    message,
                    "Field widget 'CopyClipboardText' does not support type 'integer'."
                );
            }
            other => panic!("Unexpected error kind: {other:?}"),
        }

        // CopyClipboardURL admitted on char field
        let value = fixture(
            r#"<form><field name="name" widget="CopyClipboardURL"/></form>"#,
            "form",
        )
        .unwrap();
        assert_eq!(value["nodes"][1]["widget"], "CopyClipboardURL");

        // CopyClipboardURL refused on integer field
        let error = fixture(
            r#"<form><field name="count" widget="CopyClipboardURL"/></form>"#,
            "form",
        )
        .unwrap_err();
        match error {
            ViewError::InvalidDescription(message) => {
                assert_eq!(
                    message,
                    "Field widget 'CopyClipboardURL' does not support type 'integer'."
                );
            }
            other => panic!("Unexpected error kind: {other:?}"),
        }
    }

    #[test]
    fn field_widget_placeholder_dependency_requires_text() {
        let error = fixture(
            r#"<form><field name="name" options="{'placeholder_field':'count'}"/></form>"#,
            "form",
        )
        .unwrap_err();
        match error {
            ViewError::InvalidDescription(message) => assert_eq!(
                message,
                "Invalid value for field option 'placeholder_field' on widget 'char'."
            ),
            other => panic!("Unexpected error kind: {other:?}"),
        }
    }

    #[test]
    fn field_widget_password_options_are_typed() {
        let value = fixture(r#"<form><field name="name" widget="password" placeholder="Secret" help="Keep private" autocomplete="new-password"/></form>"#, "form").unwrap();
        assert_eq!(value["nodes"][1]["widget"], "password");
        assert_eq!(value["nodes"][1]["options"]["placeholder"], "Secret");
        assert_eq!(value["nodes"][1]["options"]["help"], "Keep private");
        assert_eq!(value["nodes"][1]["options"]["autocomplete"], "new-password");
    }

    #[test]
    fn percentage_widget_admission_by_field_type() {
        let value = fixture(
            r#"<form><field name="count" widget="percentage" options="{'no_symbol': True, 'digits': [16, 2]}"/></form>"#,
            "form",
        )
        .unwrap();
        assert_eq!(value["nodes"][1]["widget"], "percentage");
        assert_eq!(value["nodes"][1]["options"]["no_symbol"], true);
        assert!(
            !value["nodes"][1]["options"]["digits"].is_null(),
            "digits option must reach the description"
        );
        let error = fixture(
            r#"<form><field name="name" widget="percentage"/></form>"#,
            "form",
        )
        .unwrap_err();
        match error {
            ViewError::InvalidDescription(message) => assert_eq!(
                message,
                "Field widget 'percentage' does not support type 'char'."
            ),
            other => panic!("Unexpected error kind: {other:?}"),
        }
    }

    #[test]
    fn field_widget_whitelist_field_refuses_completion_dependency() {
        let error = fixture(r#"<form><field name="state" widget="filterable_selection" options="{'whitelist_fname':'state'}"/></form>"#, "form").unwrap_err();
        match error {
            ViewError::InvalidDescription(message) => assert_eq!(
                message,
                "Selection whitelist field data requires a later release."
            ),
            other => panic!("Unexpected error kind: {other:?}"),
        }
    }

    #[test]
    fn radio_widget_admission_and_options() {
        let value = fixture(
            r#"<form><field name="state" widget="radio" options="{'horizontal': True}"/></form>"#,
            "form",
        )
        .unwrap();
        assert_eq!(value["nodes"][1]["widget"], "radio");
        assert_eq!(value["nodes"][1]["options"]["horizontal"], true);

        let value = fixture(
            r#"<form><field name="state" widget="radio" options="{'horizontal': False}"/></form>"#,
            "form",
        )
        .unwrap();
        assert_eq!(value["nodes"][1]["widget"], "radio");
        assert_eq!(value["nodes"][1]["options"]["horizontal"], false);

        let value = fixture(
            r#"<form><field name="state" widget="radio"/></form>"#,
            "form",
        )
        .unwrap();
        assert_eq!(value["nodes"][1]["widget"], "radio");

        let value = fixture(
            r#"<form><field name="partner_id" widget="radio"/></form>"#,
            "form",
        )
        .unwrap();
        assert_eq!(value["nodes"][1]["widget"], "radio");

        let error = fixture(
            r#"<form><field name="name" widget="radio"/></form>"#,
            "form",
        )
        .unwrap_err();
        match error {
            ViewError::InvalidDescription(message) => {
                assert_eq!(
                    message,
                    "Field widget 'radio' does not support type 'char'."
                );
            }
            other => panic!("Unexpected error kind: {other:?}"),
        }

        let error = fixture(
            r#"<form><field name="state" widget="radio" options="{'no_open': True}"/></form>"#,
            "form",
        )
        .unwrap_err();
        match error {
            ViewError::InvalidDescription(message) => {
                assert_eq!(
                    message,
                    "Field option 'no_open' is not supported by widget 'radio'."
                );
            }
            other => panic!("Unexpected error kind: {other:?}"),
        }

        let error = fixture(
            r#"<form><field name="state" widget="radio" options="{'horizontal': 'yes'}"/></form>"#,
            "form",
        )
        .unwrap_err();
        match error {
            ViewError::InvalidDescription(message) => {
                assert_eq!(
                    message,
                    "Invalid value for field option 'horizontal' on widget 'radio'."
                );
            }
            other => panic!("Unexpected error kind: {other:?}"),
        }

        let error = fixture(
            r#"<form><field name="name" widget="char" options="{'horizontal': True}"/></form>"#,
            "form",
        )
        .unwrap_err();
        match error {
            ViewError::InvalidDescription(message) => {
                assert_eq!(
                    message,
                    "Field option 'horizontal' is not supported by widget 'char'."
                );
            }
            other => panic!("Unexpected error kind: {other:?}"),
        }
    }

    #[test]
    fn code_field_modes_and_default_reach_h18_options() {
        for mode in [
            "xml",
            "python",
            "javascript",
            "json",
            "qweb",
            "scss",
            "bash",
            "text",
        ] {
            let xml = format!(
                "<form><field name=\"source\" widget=\"code\" options=\"{{'mode': '{mode}'}}\"/></form>"
            );
            let value = fixture(&xml, "form").unwrap();
            assert_eq!(value["nodes"][1]["widget"], "code");
            assert_eq!(
                value["nodes"][1]["options"],
                serde_json::json!({"mode": mode})
            );
        }
        for options in ["", " options=\"{}\""] {
            let xml = format!("<form><field name=\"source\" widget=\"code\"{options}/></form>");
            let value = fixture(&xml, "form").unwrap();
            assert_eq!(
                value["nodes"][1]["options"],
                serde_json::json!({"mode": "qweb"})
            );
        }
    }

    #[test]
    fn code_field_options_fail_with_exact_errors() {
        for (options, message) in [
            ("[]", "Code field options must be a literal dictionary."),
            (
                "dict(mode='xml')",
                "Code field options must be a literal dictionary.",
            ),
            (
                "{'mode': mode}",
                "Code field option keys and values must be string literals.",
            ),
            (
                "{'mode': None}",
                "Code field option keys and values must be string literals.",
            ),
            (
                "{'mode': 1}",
                "Code field option keys and values must be string literals.",
            ),
            (
                "{1: 'xml'}",
                "Code field option keys and values must be string literals.",
            ),
            ("{'wrap': 'xml'}", "Unknown code field option 'wrap'."),
            (
                "{'mode': 'xml', 'mode': 'json'}",
                "Duplicate code field option 'mode'.",
            ),
            ("{'mode': ''}", "Unsupported code field mode ''."),
            ("{'mode': 'XML'}", "Unsupported code field mode 'XML'."),
            ("{'mode': 'html'}", "Unsupported code field mode 'html'."),
            (
                "{'mode': 'xml' + ''}",
                "Code field option keys and values must be string literals.",
            ),
        ] {
            let xml = format!(
                "<form><field name=\"source\" widget=\"code\" options=\"{options}\"/></form>"
            );
            let error = fixture(&xml, "form").unwrap_err();
            assert_eq!(error, ViewError::InvalidDescription(message.to_owned()));
            assert_eq!(error.to_string(), message);
        }
        for (field, kind) in [
            ("name", "char"),
            ("count", "integer"),
            ("state", "selection"),
        ] {
            let xml = format!("<form><field name=\"{field}\" widget=\"code\"/></form>");
            let error = fixture(&xml, "form").unwrap_err();
            let message = format!("Field widget 'code' does not support type '{kind}'.");
            assert_eq!(error, ViewError::InvalidDescription(message.clone()));
            assert_eq!(error.to_string(), message);
        }
    }

    #[test]
    fn code_field_options_refuse_source_and_depth_limits() {
        let admitted = format!(
            "{}{{'mode': 'xml'}}",
            " ".repeat(4096 - "{'mode': 'xml'}".len())
        );
        let xml =
            format!("<form><field name=\"source\" widget=\"code\" options=\"{admitted}\"/></form>");
        let value = fixture(&xml, "form").unwrap();
        assert_eq!(
            value["nodes"][1]["options"],
            serde_json::json!({"mode":"xml"})
        );
        for (options, message) in [
            (
                format!("{admitted} "),
                "Code field options exceed 4096 UTF-8 bytes.",
            ),
            (
                format!("{{'mode': '{}'}}", "é".repeat(2048)),
                "Code field options exceed 4096 UTF-8 bytes.",
            ),
            (
                format!("{{'mode': {}'xml'{}}}", "(".repeat(20), ")".repeat(20)),
                "Code field options exceed the supported syntax bounds.",
            ),
        ] {
            let xml = format!(
                "<form><field name=\"source\" widget=\"code\" options=\"{options}\"/></form>"
            );
            let error = fixture(&xml, "form").unwrap_err();
            assert_eq!(error, ViewError::InvalidDescription(message.to_owned()));
            assert_eq!(error.to_string(), message);
        }
    }

    #[test]
    fn code_field_parser_scratch_refusal_is_sticky() {
        let source = format!("{}{{'mode': 'xml'}}", " ".repeat(4000));
        let limits = erp_sdk::expr::EvalLimits::default()
            .with_max_source_bytes(4096)
            .with_max_depth(8);
        let scratch = erp_sdk::expr::source_preflight(&source, limits)
            .unwrap()
            .reserve_bytes();
        let limit = scratch.checked_sub(1).unwrap();
        fixture_budget(
            "<form><field name=\"source\"/></form>",
            "form",
            &DescriptionBudget::with_limit(limit).unwrap(),
        )
        .unwrap();
        let budget = DescriptionBudget::with_limit(limit).unwrap();
        let xml =
            format!("<form><field name=\"source\" widget=\"code\" options=\"{source}\"/></form>");
        let error = fixture_budget(&xml, "form", &budget).unwrap_err();
        let ViewError::LimitExceeded {
            limit: kind,
            max,
            actual: Some(actual),
        } = &error
        else {
            panic!("expected parser scratch admission refusal: {error}");
        };
        assert_eq!(*kind, erp_sdk::views::LimitKind::DescriptionBuildBytes);
        assert_eq!(*max, limit);
        assert!(*actual > limit);
        assert_eq!(
            error.to_string(),
            format!(
                "simultaneously retained description bytes limit exceeded: maximum {limit}, reached {actual}"
            )
        );
        let sticky = budget.check().unwrap_err();
        assert_eq!(
            sticky,
            ViewError::LimitExceeded {
                limit: erp_sdk::views::LimitKind::DescriptionBuildBytes,
                max: limit,
                actual: None
            }
        );
        assert_eq!(
            sticky.to_string(),
            format!("simultaneously retained description bytes limit exceeded: maximum {limit}")
        );
    }

    #[test]
    fn builtin_h18_shared_lowering_budget_retains_first_view_and_refuses_pair_stickily() {
        let registry = RegistryBuilder::new([&erp_addon_base::ADDON, &crate::ADDON])
            .with_decls([&MODEL])
            .build()
            .unwrap();
        let spec = registry.model("h18.fixture").unwrap();
        let field = format!("<field name=\"name\" string=\"{}\"/>", "L".repeat(2048));
        let form_xml = format!("<form>{}</form>", field.repeat(32));
        let list_xml = format!("<list>{}</list>", field.repeat(32));
        let catalogue = ViewCatalogue::new(vec![
            ViewDescriptor::new(1, "form", spec.name, &form_xml),
            ViewDescriptor::new(2, "list", spec.name, &list_xml),
        ])
        .unwrap();
        let form = catalogue.combine(1).unwrap();
        let list = catalogue.combine(2).unwrap();
        let form_schema = form_schema().unwrap();
        let list_schema = list_schema().unwrap();
        let build_form = |budget: &DescriptionBudget| {
            DescriptionBuilder::new(&form_schema, budget)
                .build(|root| super::lower_tree(form.arch().root(), spec, "form", root))
        };
        let build_list = |budget: &DescriptionBudget| {
            DescriptionBuilder::new(&list_schema, budget)
                .build(|root| super::lower_tree(list.arch().root(), spec, "list", root))
        };
        let form_probe = build_form(&DescriptionBudget::new()).unwrap();
        let list_probe = build_list(&DescriptionBudget::new()).unwrap();
        let form_retained = form_probe.retained_bytes();
        let list_retained = list_probe.retained_bytes();
        // This derives the lower-only functional boundary from actual retained payload storage.
        // Planner scratch is additionally charged by lower_tree while each build runs. Each
        // view independently proves it fits this limit, but both retained payloads cannot fit.
        let limit = form_retained
            .checked_add(list_retained)
            .unwrap()
            .checked_sub(1)
            .unwrap();
        drop(form_probe);
        drop(list_probe);
        assert!(build_form(&DescriptionBudget::with_limit(limit).unwrap()).is_ok());
        assert!(build_list(&DescriptionBudget::with_limit(limit).unwrap()).is_ok());
        let shared = DescriptionBudget::with_limit(limit).unwrap();
        let first = build_form(&shared).unwrap();
        let error = build_list(&shared).unwrap_err();
        let ViewError::LimitExceeded {
            limit: kind,
            max,
            actual: Some(actual),
        } = &error
        else {
            panic!("expected build admission refusal: {error}")
        };
        assert_eq!(*kind, erp_sdk::views::LimitKind::DescriptionBuildBytes);
        assert_eq!(*max, limit);
        assert!(*actual > limit);
        assert_eq!(
            error.to_string(),
            format!(
                "simultaneously retained description bytes limit exceeded: maximum {limit}, reached {actual}"
            )
        );
        let sticky = DescriptionEncoder::new(&shared, &ViewLimits::default())
            .encode(&first)
            .unwrap_err();
        assert!(
            matches!(sticky,ViewError::LimitExceeded{limit:erp_sdk::views::LimitKind::DescriptionBuildBytes,max,actual:None} if max==limit)
        );
        assert_eq!(
            sticky.to_string(),
            format!("simultaneously retained description bytes limit exceeded: maximum {limit}")
        );
        eprintln!(
            "safety_fixture=form_list_shared_lowering form_retained={form_retained} list_retained={list_retained} limit={limit} refusal_actual={actual}"
        );
    }

    #[test]
    fn builtin_h18_search_field_operator_refuses_unknown_text() {
        let error = fixture(
            "<search><field name=\"name\" operator=\"unknown_operator\"/></search>",
            "search",
        )
        .unwrap_err();
        assert!(matches!(error, ViewError::InvalidDescription(_)));
        assert_eq!(
            error.to_string(),
            "Unsupported search field operator 'unknown_operator' for 'h18.fixture.name'. Additional operators require a later release."
        );
    }

    #[test]
    fn builtin_h18_extended_panel_sections_preserve_types_and_order() {
        let value = fixture(
            r#"<search><searchpanel><field name="stage"/><field name="state" select="multi" enable_counters="1"/></searchpanel></search>"#,
            "search",
        ).unwrap();
        let panel = &value["nodes"][0]["search_panel"];
        assert_eq!(panel["kind"], "sections");
        assert_eq!(panel["version"], 1);
        assert_eq!(panel["view_types"], serde_json::json!(["list", "kanban"]));
        let sections = panel["sections"].as_array().unwrap();
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0]["kind"], "category");
        assert_eq!(
            sections[0]["field"],
            serde_json::json!({"model":"h18.fixture","name":"stage"})
        );
        assert_eq!(sections[0]["id"], 1);
        assert_eq!(sections[0]["hierarchize"], true);
        assert_eq!(sections[0]["limit"], 200);
        assert_eq!(sections[0]["icon"], "folder");
        assert!(
            sections[0].get("default_values").is_none(),
            "structural callbacks cannot populate caller defaults"
        );
        assert_eq!(sections[1]["kind"], "filter");
        assert_eq!(sections[1]["enable_counters"], true);
        assert_eq!(sections[1]["id"], 2);
        assert_eq!(sections[1]["icon"], "filter_alt");
    }

    #[test]
    fn builtin_h18_extended_panel_section_count_boundary() {
        let xml = |count| {
            format!(
                "<search><searchpanel>{}</searchpanel></search>",
                "<field name='state' select='multi'/>".repeat(count)
            )
        };
        let accepted = fixture(&xml(64), "search").unwrap();
        assert_eq!(
            accepted["nodes"][0]["search_panel"]["sections"]
                .as_array()
                .unwrap()
                .len(),
            64
        );
        let error = fixture(&xml(65), "search").unwrap_err();
        assert_eq!(
            error,
            ViewError::InvalidDescription(
                "Search panel section 65 exceeds section count limit.".to_owned()
            )
        );
        assert_eq!(
            error.to_string(),
            "Search panel section 65 exceeds section count limit."
        );
    }

    #[test]
    fn builtin_h18_form_keeps_layout_and_model_scoped_fields() {
        let value = fixture("<form><header><field name=\"state\" widget=\"statusbar\" statusbar_visible=\"draft,done\"/><button name=\"confirm\" type=\"object\" string=\"Confirm\" class=\"btn-primary\"/></header><sheet><div class=\"oe_title\"><h1><field name=\"name\"/></h1></div><group><field name=\"count\"/></group><notebook><page name=\"details\" string=\"Details\"><field name=\"name\"/></page></notebook></sheet><footer><button special=\"cancel\" string=\"Cancel\"/></footer></form>", "form").unwrap();
        let nodes = value["nodes"].as_array().unwrap();
        assert_eq!(nodes[0]["kind"], "form");
        assert_eq!(nodes[0]["children"], serde_json::json!([1, 4, 13]));
        assert_eq!(
            nodes[2]["field"],
            serde_json::json!({"model":"h18.fixture","name":"state"})
        );
        assert_eq!(
            nodes[2]["options"]["visible"],
            serde_json::json!(["draft", "done"])
        );
        assert_eq!(nodes[14]["kind"], "cancel_button");
    }

    #[test]
    fn builtin_h18_list_has_limit_and_static_modifiers() {
        let value = fixture("<list limit=\"40\"><field name=\"name\" readonly=\"1\"/><field name=\"count\"/></list>", "list").unwrap();
        assert_eq!(value["nodes"][0]["page_limit"], 40);
        assert_eq!(value["nodes"][0]["children"], serde_json::json!([1, 2]));
        assert_eq!(value["nodes"][1]["modifiers"]["readonly"]["schema"], 1);
    }

    #[test]
    fn builtin_h18_list_refuses_dynamic_field_modifiers() {
        for attr in ["invisible", "readonly", "required"] {
            let xml = format!(
                "<list><field name=\"count\"/><field name=\"name\" {attr}=\"count > 1\"/></list>"
            );
            match fixture(&xml, "list").unwrap_err() {
                ViewError::InvalidDescription(message) => assert_eq!(
                    message,
                    format!(
                        "Dynamic '{attr}' modifiers on list fields need per-row evaluation, which a later release adds."
                    )
                ),
                other => panic!("Unexpected error kind: {other:?}"),
            }
        }
        fixture(
            "<list><field name=\"name\" invisible=\"1\" readonly=\"True\" required=\"0\"/></list>",
            "list",
        )
        .unwrap();
        fixture(
            "<form><field name=\"count\"/><field name=\"name\" invisible=\"count > 1\"/></form>",
            "form",
        )
        .unwrap();
    }

    #[test]
    fn builtin_h18_search_keeps_fields_filters_groups_and_expand() {
        let value = fixture("<search><field name=\"name\"/><group string=\"Filters\" expand=\"1\"><filter name=\"has_count\" string=\"Has Count\" domain=\"[('count', '&gt;', 0)]\"/><separator/></group></search>", "search").unwrap();
        assert_eq!(value["nodes"][0]["kind"], "search");
        assert_eq!(value["nodes"][2]["expand"], true);
        assert_eq!(value["nodes"][3]["domain"]["schema"], 1);
    }

    #[test]
    fn builtin_h18_button_invisible_checks_ast_and_shown_fields() {
        let value = fixture("<form><header><button name=\"confirm\" type=\"object\" string=\"Confirm\" invisible=\"count &gt; 0\"/></header><field name=\"count\"/></form>", "form").unwrap();
        assert_eq!(value["nodes"][2]["modifiers"]["invisible"]["schema"], 1);
        for (source, expected) in [
            (
                "name",
                "Button 'confirm' invisible expression uses 'name', which is not a field shown by this view. Unsupported form layout requires a later release.",
            ),
            (
                "count.real",
                "Button invisible expressions support names, literals, comparisons, not/and/or, arithmetic and conditionals only. Unsupported form layout requires a later release.",
            ),
            (
                "check(count)",
                "Button invisible expressions support names, literals, comparisons, not/and/or, arithmetic and conditionals only. Unsupported form layout requires a later release.",
            ),
        ] {
            let xml = format!(
                "<form><header><button name=\"confirm\" type=\"object\" string=\"Confirm\" invisible=\"{source}\"/></header><field name=\"count\"/></form>"
            );
            let error = fixture(&xml, "form").unwrap_err();
            assert!(matches!(error, ViewError::InvalidDescription(_)));
            assert_eq!(error.to_string(), expected);
        }
    }

    #[test]
    fn builtin_h18_refuses_unknown_attributes_instead_of_ignoring() {
        let error = fixture("<list><field name=\"name\" bogus=\"1\"/></list>", "list").unwrap_err();
        assert!(matches!(error, ViewError::InvalidDescription(_)));
        assert_eq!(
            error.to_string(),
            "View attribute 'bogus' is unsupported by the current human UI contract."
        );
    }

    #[test]
    fn builtin_h18_list_action_attributes_lower_booleans() {
        let value = fixture(
            "<list create=\"0\" edit=\"false\" delete=\"1\"><field name=\"name\"/></list>",
            "list",
        )
        .unwrap();
        let root = &value["nodes"][0];
        assert_eq!(root["create"], false);
        assert_eq!(root["edit"], false);
        assert_eq!(root["delete"], true);

        let error = fixture(
            "<list create=\"invalid\"><field name=\"name\"/></list>",
            "list",
        )
        .unwrap_err();
        assert!(matches!(error, ViewError::InvalidDescription(_)));
        assert_eq!(
            error.to_string(),
            "Dynamic 'create' modifier is unsupported by the current human UI contract."
        );
    }

    #[test]
    fn builtin_h18_form_action_attributes_lower_booleans() {
        let value = fixture(
            "<form create=\"false\" edit=\"0\" delete=\"true\"><field name=\"name\"/></form>",
            "form",
        )
        .unwrap();
        let root = &value["nodes"][0];
        assert_eq!(root["create"], false);
        assert_eq!(root["edit"], false);
        assert_eq!(root["delete"], true);

        let error =
            fixture("<form edit=\"maybe\"><field name=\"name\"/></form>", "form").unwrap_err();
        assert!(matches!(error, ViewError::InvalidDescription(_)));
        assert_eq!(
            error.to_string(),
            "Dynamic 'edit' modifier is unsupported by the current human UI contract."
        );
    }

    #[test]
    fn builtin_h18_static_invisible_prunes_complete_layout_subtrees() {
        let value = fixture("<form><group invisible=\"1\"><field name=\"name\"/></group><group><field name=\"count\"/></group></form>", "form").unwrap();
        assert_eq!(value["nodes"].as_array().unwrap().len(), 3);
        assert_eq!(value["nodes"][0]["children"], serde_json::json!([1]));
        assert_eq!(value["nodes"][1]["children"], serde_json::json!([2]));
    }
    #[test]
    fn builtin_h18_selection_panel_is_preserved_on_standalone_search() {
        let value=fixture("<search><searchpanel><field name=\"state\" enable_counters=\"1\" expand=\"1\"/></searchpanel></search>","search").unwrap();
        let panel = &value["nodes"][0]["search_panel"];
        assert_eq!(panel["kind"], "selection");
        assert_eq!(
            panel["field"],
            serde_json::json!({"model":"h18.fixture","name":"state"})
        );
        assert_eq!(panel["enable_counters"], true);
        assert_eq!(panel["expand"], true);
    }
    #[test]
    fn builtin_h18_extended_panel_preserves_filter_and_view_types() {
        let value = fixture(
            "<search><searchpanel><field name=\"state\" select=\"multi\"/></searchpanel></search>",
            "search",
        )
        .unwrap();
        assert_eq!(value["nodes"][0]["search_panel"]["kind"], "sections");
        assert_eq!(
            value["nodes"][0]["search_panel"]["sections"][0]["kind"],
            "filter"
        );
        let value=fixture("<search><searchpanel view_types=\"kanban\"><field name=\"state\"/></searchpanel></search>","search").unwrap();
        assert_eq!(
            value["nodes"][0]["search_panel"]["view_types"],
            serde_json::json!(["kanban"])
        );
    }

    #[test]
    fn builtin_h18_hidden_layout_still_refuses_unknown_element_structure() {
        let error = fixture(
            "<form><group invisible=\"1\"><script/></group></form>",
            "form",
        )
        .unwrap_err();
        assert!(matches!(error, ViewError::InvalidDescription(_)));
        assert_eq!(
            error.to_string(),
            "View element <script> is not allowed under <group>. Unsupported form layout requires a later release."
        );
    }

    #[test]
    fn builtin_h18_panel_failure_boundaries_preserve_exact_descriptors() {
        for (xml, message) in [
            (
                "<search><searchpanel bogus=\"1\"><field name=\"state\"/></searchpanel></search>",
                "Search panel attribute 'bogus' is not supported. Supported search-panel extensions require a later release.",
            ),
            (
                "<search><searchpanel><field name=\"state\" enable_counters=\"dynamic\"/></searchpanel></search>",
                "Dynamic 'enable_counters' modifier is unsupported by the current human UI contract. Supported search-panel extensions require a later release.",
            ),
            (
                "<search><searchpanel><field name=\"name\"/></searchpanel></search>",
                "Search panel selection category requires a static selection field. Supported search-panel extensions require a later release.",
            ),
            (
                "<search><searchpanel/><searchpanel/></search>",
                "Only one search panel is supported. Supported search-panel extensions require a later release.",
            ),
        ] {
            let value = fixture(xml, "search").unwrap();
            assert_eq!(value["nodes"][0]["search_panel"]["kind"], "unsupported");
            assert_eq!(value["nodes"][0]["search_panel"]["message"], message);
        }
        let error = fixture(
            "<search><searchpanel><field name=\"state\" limit=\"wrong\"/></searchpanel></search>",
            "search",
        )
        .unwrap_err();
        assert!(matches!(error, ViewError::InvalidDescription(_)));
        assert_eq!(
            error.to_string(),
            "Search panel limit must be a nonnegative integer."
        );
    }
    #[test]
    fn builtin_h18_empty_search_groups_do_not_leave_orphan_nodes() {
        let value=fixture("<search><field name=\"name\"/><group><filter name=\"hidden\" invisible=\"1\"/></group><group string=\"Visible\"><separator/></group></search>","search").unwrap();
        assert_eq!(value["nodes"].as_array().unwrap().len(), 4);
        assert_eq!(value["nodes"][0]["children"], serde_json::json!([1, 2]));
        assert_eq!(value["nodes"][2]["children"], serde_json::json!([3]));
    }
    #[test]
    fn builtin_h18_wire_attrs_buttons_reach_actual_output() {
        let xml = r#"<form><header><button name='confirm' type='object' string='' title='Confirm record' help='Help' class='btn-success rounded-circle addon_hook' size='sm' icon='lucide:refresh-cw' data-hotkey='shift+x' debounce='60000' block-ui='1' confirm='Continue?' confirm-title='Review' confirm-label='Proceed' cancel-label='Stay' context="{'default_count': count}"/></header><field name='count'/></form>"#;
        let output = fixture(xml, "form").unwrap();
        let button = &output["nodes"][2];
        assert_eq!(button["variant"], "success");
        let mut details = button["details"].as_object().unwrap().clone();
        let context = details
            .remove("context")
            .expect("context expression retained");
        assert_eq!(
            Value::Object(details),
            serde_json::json!({"size":"small","circle":true,"classes":["addon_hook"],"icon":"lucide:refresh-cw","title":"Confirm record","help":"Help","hotkey":"shift+x","debounce_ms":60000,"block_ui":true,"confirm_title":"Review","confirm_label":"Proceed","cancel_label":"Stay"})
        );
        assert_eq!(context["schema"], 1);
        for (attributes, variant, target) in [
            (
                "name='7' type='action' class='btn-link'",
                "link",
                serde_json::json!({"kind":"action","id":7}),
            ),
            (
                "type='url' href='/documentation'",
                "secondary",
                serde_json::json!({"kind":"url","href":"/documentation"}),
            ),
            (
                "class='addon_hook'",
                "unstyled",
                serde_json::json!({"kind":"inert"}),
            ),
        ] {
            let xml = format!("<form><header><button {attributes} string='Open'/></header></form>");
            let output = fixture(&xml, "form").unwrap();
            assert_eq!(output["nodes"][2]["kind"], "action_button");
            assert_eq!(output["nodes"][2]["variant"], variant);
            assert_eq!(output["nodes"][2]["target"], target);
        }
        let output = fixture("<form><sheet><div name='button_box'><button name='confirm' type='object' class='oe_stat_button btn-info'><field name='count' widget='statinfo' string='Count'/></button></div></sheet><footer><button special='cancel' string='Close' size='lg'/></footer></form>", "form").unwrap();
        assert_eq!(output["nodes"][3]["kind"], "stat_button");
        assert_eq!(output["nodes"][3]["variant"], "info");
        assert_eq!(output["nodes"][6]["details"]["size"], "large");
    }

    #[test]
    fn builtin_h18_wire_attrs_display_leaves_and_refusals() {
        for (attrs, text, background) in [
            ("title='Preferred' text='Fallback'", "Preferred", "success"),
            (
                "text='Fallback' bg_color='bg-info' tooltip='Status'",
                "Fallback",
                "info",
            ),
            (
                "title='Watch' bg_color='text-bg-warning'",
                "Watch",
                "warning",
            ),
        ] {
            let xml = format!(
                "<form><sheet><group><widget name='web_ribbon' {attrs}/><widget name='notification_alert'/></group></sheet></form>"
            );
            let output = fixture(&xml, "form").unwrap();
            assert_eq!(output["nodes"][3]["kind"], "ribbon");
            assert_eq!(output["nodes"][3]["text"], text);
            assert_eq!(output["nodes"][3]["background"], background);
            assert_eq!(
                output["nodes"][4],
                serde_json::json!({"kind":"notification_alert"})
            );
        }
        for (attrs, expected) in [
            ("size='xl'", "Invalid button size 'xl'."),
            ("data-hotkey='ctrl+x'", "Invalid button hotkey 'ctrl+x'."),
            (
                "debounce='60001'",
                "Button debounce must be an integer from 0 to 60000.",
            ),
            (
                "confirm-label='Proceed'",
                "Button confirmation labels require confirm.",
            ),
        ] {
            let xml = format!(
                "<form><header><button name='confirm' type='object' string='Confirm' {attrs}/></header></form>"
            );
            let error = fixture(&xml, "form").unwrap_err();
            assert_eq!(error, ViewError::InvalidDescription(expected.to_owned()));
            assert_eq!(error.to_string(), expected);
        }
        let error = fixture(
            "<form><widget name='web_ribbon' title='Status' bg_color='bg-purple'/></form>",
            "form",
        )
        .unwrap_err();
        assert_eq!(
            error,
            ViewError::InvalidDescription("Unsupported ribbon background 'bg-purple'.".to_owned())
        );
        assert_eq!(
            error.to_string(),
            "Unsupported ribbon background 'bg-purple'."
        );
    }
    fn attribute_button(extra: &str) -> Result<Value, ViewError> {
        fixture(
            &format!(
                "<form><header><button name='confirm' type='object' string='Confirm' {extra}/></header></form>"
            ),
            "form",
        )
    }
    fn assert_attribute_refusal(result: Result<Value, ViewError>, expected: &str) {
        let error = result.unwrap_err();
        assert_eq!(error, ViewError::InvalidDescription(expected.to_owned()));
        assert_eq!(error.to_string(), expected);
    }

    #[test]
    fn builtin_h18_attribute_rank_and_size_boundaries() {
        for (class, rank) in [
            (None, "secondary"),
            (Some("addon_hook"), "unstyled"),
            (Some(""), "unstyled"),
            (Some("btn-primary"), "primary"),
            (Some("oe_highlight"), "primary"),
            (Some("btn-secondary"), "secondary"),
            (Some("btn-link"), "link"),
            (Some("btn-success"), "success"),
            (Some("btn-info"), "info"),
            (Some("btn-warning"), "warning"),
            (Some("btn-danger"), "danger"),
        ] {
            for (size, expected) in [("sm", "small"), ("md", "normal"), ("lg", "large")] {
                let class = class
                    .map(|value| format!("class='{value}'"))
                    .unwrap_or_default();
                let output = attribute_button(&format!("{class} size='{size}'")).unwrap();
                assert_eq!(output["nodes"][2]["variant"], rank);
                assert_eq!(output["nodes"][2]["details"]["size"], expected);
            }
        }
        assert_attribute_refusal(
            attribute_button("class='btn-primary btn-danger'"),
            "Conflicting button rank classes.",
        );
        assert_attribute_refusal(attribute_button("size='SM'"), "Invalid button size 'SM'.");
    }

    #[test]
    fn builtin_h18_attribute_class_byte_and_token_boundaries() {
        let at_limit = format!(
            " {}",
            (0..8)
                .map(|i| format!("a{i}{}", "a".repeat(61)))
                .collect::<Vec<_>>()
                .join(" ")
        );
        assert_eq!(at_limit.len(), 512);
        let output = attribute_button(&format!("class='{at_limit}'")).unwrap();
        assert_eq!(
            output["nodes"][2]["details"]["classes"]
                .as_array()
                .unwrap()
                .len(),
            8
        );
        let too_long = format!("{at_limit} ");
        assert_eq!(too_long.len(), 513);
        for raw in [too_long, "a b c d e f g h i".to_owned()] {
            assert_attribute_refusal(
                attribute_button(&format!("class='{raw}'")),
                "Button class exceeds its 512-byte/eight-token bound.",
            );
        }
        for raw in ["a b c d e f g h".to_owned(), "a".repeat(64)] {
            let output = attribute_button(&format!("class='{raw}'")).unwrap();
            let actual = output["nodes"][2]["details"]["classes"].as_array().unwrap();
            assert_eq!(
                actual
                    .iter()
                    .map(|value| value.as_str().unwrap())
                    .collect::<Vec<_>>(),
                raw.split(' ').collect::<Vec<_>>()
            );
        }
        for raw in [
            "a".repeat(65),
            "é".to_owned(),
            "1bad".to_owned(),
            "a:b".to_owned(),
            "a&#9;b".to_owned(),
            "a&#10;b".to_owned(),
        ] {
            assert_attribute_refusal(
                attribute_button(&format!("class='{raw}'")),
                "Invalid button class token.",
            );
        }
        assert_attribute_refusal(
            attribute_button("class='same same'"),
            "Duplicate button class token.",
        );
    }

    #[test]
    fn builtin_h18_attribute_text_confirmation_and_icon_boundaries() {
        let at_limit = "é".repeat(2048);
        let too_long = format!("{at_limit}a");
        assert_eq!((at_limit.len(), too_long.len()), (4096, 4097));
        for (xml, wire) in [
            ("title", "title"),
            ("help", "help"),
            ("confirm", "confirm"),
            ("confirm-title", "confirm_title"),
            ("confirm-label", "confirm_label"),
            ("cancel-label", "cancel_label"),
        ] {
            let prerequisite = if xml == "confirm" {
                ""
            } else {
                "confirm='Continue?'"
            };
            let output = attribute_button(&format!("{prerequisite} {xml}='{at_limit}'")).unwrap();
            let button = &output["nodes"][2];
            let actual = if xml == "confirm" {
                &button["confirm"]
            } else {
                &button["details"][wire]
            };
            assert_eq!(actual.as_str(), Some(at_limit.as_str()), "{xml}");
            assert_attribute_refusal(
                attribute_button(&format!("{prerequisite} {xml}='{too_long}'")),
                &format!("Button '{xml}' exceeds the 4096-byte limit."),
            );
            if xml.starts_with("confirm") || xml == "cancel-label" {
                assert_attribute_refusal(
                    attribute_button(&format!("{prerequisite} {xml}='   '")),
                    &format!("Button '{xml}' cannot be empty."),
                );
            }
        }
        let icon = format!("lucide:{}", "a".repeat(249));
        assert_eq!(icon.len(), 256);
        let output = attribute_button(&format!("icon='{icon}'")).unwrap();
        assert_eq!(output["nodes"][2]["details"]["icon"], icon);
        // Native admission bounds the identity grammar; shipped manifest membership is client-owned.
        for icon in [
            format!("{icon}a"),
            "lucide:".to_owned(),
            "fa:check".to_owned(),
            "lucide:Upper".to_owned(),
            "lucide:a_b".to_owned(),
            "lucide:é".to_owned(),
        ] {
            assert_attribute_refusal(
                attribute_button(&format!("icon='{icon}'")),
                "Invalid button icon identity.",
            );
        }
    }

    #[test]
    fn builtin_h18_attribute_debounce_boundaries() {
        for millis in [0, 60000] {
            let output = attribute_button(&format!("debounce='{millis}'")).unwrap();
            assert_eq!(output["nodes"][2]["details"]["debounce_ms"], millis);
        }
        for raw in ["-1", "60001", "4294967296", "1.5", "dynamic", ""] {
            assert_attribute_refusal(
                attribute_button(&format!("debounce='{raw}'")),
                "Button debounce must be an integer from 0 to 60000.",
            );
        }
    }

    #[test]
    fn builtin_h18_attribute_display_leaf_boundaries() {
        let at_limit = "é".repeat(2048);
        let too_long = format!("{at_limit}a");
        for key in ["title", "text", "tooltip"] {
            let title = if key == "tooltip" {
                "title='Ribbon'"
            } else {
                ""
            };
            let output = fixture(
                &format!("<form><widget name='web_ribbon' {title} {key}='{at_limit}'/></form>"),
                "form",
            )
            .unwrap();
            let wire_key = if key == "tooltip" { "tooltip" } else { "text" };
            assert_eq!(
                output["nodes"][1][wire_key].as_str(),
                Some(at_limit.as_str())
            );
            assert_attribute_refusal(
                fixture(
                    &format!("<form><widget name='web_ribbon' {title} {key}='{too_long}'/></form>"),
                    "form",
                ),
                &format!("Ribbon '{key}' exceeds the 4096-byte limit."),
            );
        }
        for (open, close) in [
            ("", ""),
            ("<sheet>", "</sheet>"),
            ("<group>", "</group>"),
            ("<notebook><page name='display'>", "</page></notebook>"),
            ("<aside>", "</aside>"),
        ] {
            let output = fixture(&format!("<form>{open}<widget name='web_ribbon' title='Ribbon'/><widget name='notification_alert'/>{close}</form>"), "form").unwrap();
            let nodes = output["nodes"].as_array().unwrap();
            assert!(nodes.iter().any(|node| node["kind"] == "ribbon"));
            assert!(
                nodes
                    .iter()
                    .any(|node| node["kind"] == "notification_alert")
            );
        }
        for name in ["web_ribbon", "notification_alert"] {
            assert_attribute_refusal(
                fixture(
                    &format!("<form record_mode='service'><widget name='{name}'/></form>"),
                    "form",
                ),
                &format!("Service widget '{name}' is not registered."),
            );
        }
        for (xml, expected) in [
            (
                "<form><header><widget name='notification_alert'/></header></form>",
                "View element <widget> is not allowed under <header>. Unsupported form layout requires a later release.",
            ),
            (
                "<form><widget name='web.addons'/></form>",
                "Unregistered record-form display widget.",
            ),
            (
                "<form><widget name='web_ribbon' title='Ribbon' invisible='0'/></form>",
                "View attribute 'invisible' is unsupported by the current human UI contract.",
            ),
            (
                "<form><widget name='notification_alert' permission='denied'/></form>",
                "View attribute 'permission' is unsupported by the current human UI contract.",
            ),
            (
                "<form><widget name='web_ribbon' title='Ribbon'><field name='name'/></widget></form>",
                "Display widgets cannot contain child nodes.",
            ),
        ] {
            assert_attribute_refusal(fixture(xml, "form"), expected);
        }
    }

    #[test]
    fn builtin_h18_relation_field_nolabel_and_context() {
        let output = fixture(
            "<form><field name='partner_id' nolabel='1'/><field name='child_ids' nolabel='1' context=\"{'default_parent_id': id}\"/></form>",
            "form",
        )
        .unwrap();
        let nodes = output["nodes"].as_array().unwrap();
        assert_eq!(nodes[1]["options"]["nolabel"], true);
        assert_eq!(nodes[2]["options"]["nolabel"], true);
        assert!(nodes[2]["options"]["context"].is_object());

        assert_attribute_refusal(
            fixture(
                "<form><field name='child_ids' nolabel='not_a_bool'/></form>",
                "form",
            ),
            "Dynamic 'nolabel' modifier is unsupported by the current human UI contract.",
        );
        assert_attribute_refusal(
            fixture("<form><field name='count' nolabel='1'/></form>", "form"),
            "Field option 'nolabel' is not supported by widget 'integer'.",
        );
    }

    fn chatter_fixture(model: &str, xml: &str) -> Result<Value, ViewError> {
        fixture_model(model, xml, "form", &DescriptionBudget::new())
    }

    fn chatter_message(result: Result<Value, ViewError>) -> String {
        match result.unwrap_err() {
            ViewError::InvalidDescription(message) => message,
            other => panic!("Unexpected error kind: {other:?}"),
        }
    }

    #[test]
    fn chatter_lowers_to_a_closed_node_with_all_flags() {
        let plain = chatter_fixture("h18.threaded", "<form><sheet/><chatter/></form>").unwrap();
        assert_eq!(
            plain["nodes"][2],
            serde_json::json!({
                "kind": "chatter",
                "reload_on_post": false,
                "reload_on_attachment": false,
                "reload_on_activity": false,
                "reload_on_follower": false,
                "open_attachments": false,
                "has_activities": false
            })
        );
        for (truthy, falsy) in [("1", "0"), ("true", "false"), ("True", "False")] {
            let xml = format!(
                "<form><sheet/><aside/><chatter reload_on_post='{truthy}' reload_on_attachment='{falsy}' \
                 reload_on_activity='{truthy}' reload_on_follower='{falsy}' open_attachments='{truthy}'/></form>"
            );
            let value = chatter_fixture("h18.threaded", &xml).unwrap();
            assert_eq!(
                value["nodes"][3],
                serde_json::json!({
                    "kind": "chatter",
                    "reload_on_post": true,
                    "reload_on_attachment": false,
                    "reload_on_activity": true,
                    "reload_on_follower": false,
                    "open_attachments": true,
                    "has_activities": false
                })
            );
        }
        let none = chatter_fixture("h18.threaded", "<form><sheet/></form>").unwrap();
        assert!(
            none["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .all(|node| node["kind"] != "chatter")
        );
    }

    #[test]
    fn chatter_refusals_name_kind_and_message() {
        const SUFFIX: &str = " Unsupported form layout requires a later release.";
        for flag in [
            "reload_on_post",
            "reload_on_attachment",
            "reload_on_activity",
            "reload_on_follower",
            "open_attachments",
        ] {
            for bad in ["yes", "", "TRUE", "2", " 1"] {
                let xml = format!("<form><chatter {flag}='{bad}'/></form>");
                assert_eq!(
                    chatter_message(chatter_fixture("h18.threaded", &xml)),
                    format!(
                        "Chatter flag '{flag}' must be one of 1, 0, true, false, True or False."
                    ),
                    "{flag}={bad:?}"
                );
            }
        }
        for (xml, message) in [
            (
                "<form><chatter/><chatter/></form>",
                "A record form allows at most one <chatter>.".to_owned(),
            ),
            (
                "<form><sheet><chatter/></sheet></form>",
                "View element <chatter> is not allowed under <sheet>.".to_owned(),
            ),
            (
                "<form><aside><chatter/></aside></form>",
                "View element <chatter> is not allowed under <aside>.".to_owned(),
            ),
            (
                "<form><sheet><group><chatter/></group></sheet></form>",
                "View element <chatter> is not allowed under <group>.".to_owned(),
            ),
            (
                "<form><sheet><notebook><page name='p'><chatter/></page></notebook></sheet></form>",
                "View element <chatter> is not allowed under <page>.".to_owned(),
            ),
            (
                "<form record_mode='service'><chatter/></form>",
                "View element <chatter> is not allowed under <form>.".to_owned(),
            ),
            (
                "<form><chatter><field name='name'/></chatter></form>",
                "View element <field> is not allowed under <chatter>.".to_owned(),
            ),
        ] {
            assert_eq!(
                chatter_message(chatter_fixture("h18.threaded", xml)),
                format!("{message}{SUFFIX}"),
                "{xml}"
            );
        }
        for attribute in [
            "groups='base.group_user'",
            "invisible='1'",
            "class='x'",
            "has_activities='1'",
        ] {
            let name = attribute.split('=').next().unwrap();
            assert_eq!(
                chatter_message(chatter_fixture(
                    "h18.scheduled",
                    &format!("<form><chatter {attribute}/></form>")
                )),
                format!("View attribute '{name}' is unsupported by the current human UI contract.")
            );
        }
    }

    /// Test stand-in for the host's group filter: a bare `<t>` is unwrapped, as `host_access` decides
    /// for a permitted `<t groups>` (its attributes are gone by then).
    #[derive(Clone, Copy)]
    struct Unwrapping<'a>(erp_sdk::views::NodeRef<'a>);
    impl<'a> super::Node<'a> for Unwrapping<'a> {
        fn tag(self) -> &'a str {
            self.0.tag()
        }
        fn attr(self, key: &str) -> Option<&'a str> {
            (self.0.tag() != "t").then(|| self.0.attr(key)).flatten()
        }
        fn attributes(self) -> impl Iterator<Item = (&'a str, &'a str)> + 'a {
            let wrapper = self.0.tag() == "t";
            self.0.attributes().filter(move |_| !wrapper)
        }
        fn children(self) -> impl Iterator<Item = Self> + 'a {
            self.0.children().map(Unwrapping)
        }
        fn content_text(self) -> impl Iterator<Item = super::ViewResult<&'a str>> + 'a {
            std::iter::once(Ok(""))
        }
        fn is_unwrapped(self) -> bool {
            self.0.tag() == "t"
        }
        fn source_ordinal(self) -> super::ViewResult<usize> {
            super::Node::source_ordinal(self.0)
        }
        fn source_child_count(self) -> usize {
            self.0.children().count()
        }
    }

    #[test]
    fn chatter_inside_a_conditional_wrapper_is_refused() {
        let registry = RegistryBuilder::new([&erp_addon_base::ADDON, &crate::ADDON])
            .with_decls([&MODEL, &THREAD, &ACTIVITY, &THREADED, &SCHEDULED])
            .build()
            .unwrap();
        let spec = registry.model("h18.threaded").unwrap();
        for xml in [
            r#"<form><sheet/><chatter/><t groups="base.group_user"><chatter/></t></form>"#,
            r#"<form><t groups="base.group_user"><chatter/></t></form>"#,
        ] {
            let catalogue =
                ViewCatalogue::new(vec![ViewDescriptor::new(1, "fixture", spec.name, xml)])
                    .unwrap();
            let combined = catalogue.combine(1).unwrap();
            let error = super::structure(
                Unwrapping(combined.arch().root()),
                None,
                spec,
                "form",
                false,
            )
            .unwrap_err();
            match error {
                ViewError::InvalidDescription(message) => assert_eq!(
                    message,
                    "<chatter> cannot be placed inside a conditional wrapper. Unsupported form layout requires a later release.",
                    "{xml}"
                ),
                other => panic!("Unexpected error kind: {other:?}"),
            }
        }
    }

    #[test]
    fn chatter_requires_the_thread_mixin_and_derives_activities() {
        assert_eq!(
            chatter_message(chatter_fixture("h18.fixture", "<form><chatter/></form>")),
            "View element <chatter> requires model 'h18.fixture' to inherit 'mail.thread'."
        );
        let threaded = chatter_fixture("h18.threaded", "<form><chatter/></form>").unwrap();
        assert_eq!(threaded["nodes"][1]["has_activities"], false);
        let scheduled = chatter_fixture("h18.scheduled", "<form><chatter/></form>").unwrap();
        assert_eq!(scheduled["nodes"][1]["has_activities"], true);
    }
}
