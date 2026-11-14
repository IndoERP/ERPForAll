//! Builtin closed description schemas for the `list`, `form`, `search` and `pivot` view types.
//!
//! Each payload is the wire arena of `wire::View` minus its envelope: `{root, nodes}`, where every
//! node is a tagged union on `kind` whose variants mirror the `wire::ViewNode` shapes of that view
//! type (member names and kinds, same optional members). Differences from the wire form, all
//! forced by the closed schema vocabulary:
//! - the wire's required-but-nullable members (`modifiers.*`, `confirm`, `default_value`, settings `app`,
//!   `notice`) are optional here, absent meaning null; the host envelope adds the nulls back;
//! - closed wire vocabularies (`record_mode`, `button_type`, `variant`, `widget`) are bounded text
//!   here; the typed wire decode remains their authority;
//! - ids and counters are signed 64-bit integers; the wire's u32/JSON-safe ranges are checked when
//!   the envelope is decoded as `wire::View`.
//!
//! Field references are `FieldHandle` slots (explicit model scope) and expressions are scoped to
//! the checked input's root model, so the derived visitor reports them all.
use crate::{
    ExpressionKind, ReferenceScope, SchemaNode, SchemaProperty, SchemaVariant, ViewResult,
    ViewSchema,
};

const REVISION: u16 = 1;
const MAX_NODES: usize = 4096;

static INT: SchemaNode = SchemaNode::Integer;
static BOOL: SchemaNode = SchemaNode::Bool;
static SHORT: SchemaNode = SchemaNode::Text { max_bytes: 256 };
static LABEL: SchemaNode = SchemaNode::Text { max_bytes: 4096 };
static SHORTS: SchemaNode = SchemaNode::Array {
    item: &SHORT,
    max_items: 256,
};
static CHILDREN: SchemaNode = SchemaNode::Array {
    item: &INT,
    max_items: MAX_NODES,
};
static FIELD_REF: SchemaNode = SchemaNode::FieldHandle;
static VALUE_EXPR: SchemaNode = SchemaNode::Expression {
    kind: ExpressionKind::Value,
    scope: ReferenceScope::Root,
    parent_model: None,
};
static DOMAIN_EXPR: SchemaNode = SchemaNode::Expression {
    kind: ExpressionKind::Domain,
    scope: ReferenceScope::Root,
    parent_model: None,
};
static CONTEXT_EXPR: SchemaNode = SchemaNode::Expression {
    kind: ExpressionKind::Context,
    scope: ReferenceScope::Root,
    parent_model: None,
};

// ---- shared member shapes ----------------------------------------------------------------------

static MODIFIERS: SchemaNode = SchemaNode::Object(&[
    SchemaProperty::optional("readonly", &VALUE_EXPR),
    SchemaProperty::optional("required", &VALUE_EXPR),
    SchemaProperty::optional("invisible", &VALUE_EXPR),
]);
static DIGITS: SchemaNode = SchemaNode::Array {
    item: &INT,
    max_items: 2,
};
static FIELD_OPTIONS: SchemaNode = SchemaNode::Object(&[
    SchemaProperty::optional("visible", &SHORTS),
    SchemaProperty::optional("mode", &SHORT),
    SchemaProperty::optional("help", &LABEL),
    SchemaProperty::optional("placeholder", &LABEL),
    SchemaProperty::optional("autocomplete", &LABEL),
    SchemaProperty::optional("icon", &LABEL),
    SchemaProperty::optional("icon_class", &LABEL),
    SchemaProperty::optional("min_date", &LABEL),
    SchemaProperty::optional("max_date", &LABEL),
    SchemaProperty::optional("min_precision", &LABEL),
    SchemaProperty::optional("max_precision", &LABEL),
    SchemaProperty::optional("warn", &LABEL),
    SchemaProperty::optional("type", &LABEL),
    SchemaProperty::optional("text", &LABEL),
    SchemaProperty::optional("password", &BOOL),
    SchemaProperty::optional("nolabel", &BOOL),
    SchemaProperty::optional("no_open", &BOOL),
    SchemaProperty::optional("no_create", &BOOL),
    SchemaProperty::optional("no_quick_create", &BOOL),
    SchemaProperty::optional("no_create_edit", &BOOL),
    SchemaProperty::optional("no_symbol", &BOOL),
    SchemaProperty::optional("enable_formatting", &BOOL),
    SchemaProperty::optional("hide_trailing_zeros", &BOOL),
    SchemaProperty::optional("field_digits", &BOOL),
    SchemaProperty::optional("show_time", &BOOL),
    SchemaProperty::optional("show_seconds", &BOOL),
    SchemaProperty::optional("always_range", &BOOL),
    SchemaProperty::optional("numeric", &BOOL),
    SchemaProperty::optional("create", &BOOL),
    SchemaProperty::optional("can_open", &BOOL),
    SchemaProperty::optional("horizontal", &BOOL),
    SchemaProperty::optional("rows", &INT),
    SchemaProperty::optional("max_length", &INT),
    SchemaProperty::optional("badge_limit", &INT),
    SchemaProperty::optional("tag_limit", &INT),
    SchemaProperty::optional("search_threshold", &INT),
    SchemaProperty::optional("rounding", &INT),
    SchemaProperty::optional("placeholder_field", &FIELD_REF),
    SchemaProperty::optional("currency_field", &FIELD_REF),
    SchemaProperty::optional("start_date_field", &FIELD_REF),
    SchemaProperty::optional("end_date_field", &FIELD_REF),
    SchemaProperty::optional("whitelist_fname", &FIELD_REF),
    SchemaProperty::optional("color_field", &FIELD_REF),
    SchemaProperty::optional("whitelisted_values", &SHORTS),
    SchemaProperty::optional("blacklisted_values", &SHORTS),
    SchemaProperty::optional("digits", &DIGITS),
    // Relation criterion LHS paths are authorized against the occurrence comodel by H18.
    // This visitor still derives every root-record RHS expression dependency.
    SchemaProperty::optional("domain", &VALUE_EXPR),
    SchemaProperty::optional("context", &VALUE_EXPR),
]);
static WIDGET_OPTIONS: SchemaNode = SchemaNode::Object(&[
    SchemaProperty::optional("scope_kinds", &SHORTS),
    SchemaProperty::optional("app", &SHORT),
]);
static PANEL_SELECTION: SchemaNode = SchemaNode::Object(&[
    SchemaProperty::required("field", &FIELD_REF),
    SchemaProperty::required("label", &LABEL),
    SchemaProperty::required("enable_counters", &BOOL),
    SchemaProperty::required("expand", &BOOL),
    SchemaProperty::optional("default_value", &LABEL),
    SchemaProperty::optional("notice", &LABEL),
]);
static PANEL_UNSUPPORTED: SchemaNode =
    SchemaNode::Object(&[SchemaProperty::required("message", &LABEL)]);
// These are bounded construction intermediates. The borrowed builtin-wire preflight
// applies the public panel ceilings (64 entries, label 256 bytes, icon/key component
// 64 bytes) before copying panel members, so failures retain a numeric section ID.
static PANEL_DOMAIN: SchemaNode = SchemaNode::ScopedDomain;
static PANEL_VIEW_TYPES: SchemaNode = SchemaNode::Array {
    item: &LABEL,
    max_items: MAX_NODES,
};
static PANEL_CATEGORY: SchemaNode = SchemaNode::Object(&[
    SchemaProperty::required("id", &INT),
    SchemaProperty::required("field", &FIELD_REF),
    SchemaProperty::required("label", &LABEL),
    SchemaProperty::optional("icon", &LABEL),
    SchemaProperty::required("enable_counters", &BOOL),
    SchemaProperty::required("expand", &BOOL),
    SchemaProperty::required("limit", &INT),
    SchemaProperty::required("hierarchize", &BOOL),
]);
static PANEL_FILTER: SchemaNode = SchemaNode::Object(&[
    SchemaProperty::required("id", &INT),
    SchemaProperty::required("field", &FIELD_REF),
    SchemaProperty::required("label", &LABEL),
    SchemaProperty::optional("icon", &LABEL),
    SchemaProperty::required("enable_counters", &BOOL),
    SchemaProperty::required("expand", &BOOL),
    SchemaProperty::required("limit", &INT),
    SchemaProperty::optional("domain", &PANEL_DOMAIN),
    SchemaProperty::optional("group_by", &FIELD_REF),
]);
static PANEL_SECTION: SchemaNode = SchemaNode::TaggedUnion {
    discriminator: "kind",
    variants: &[
        SchemaVariant::new("category", &PANEL_CATEGORY),
        SchemaVariant::new("filter", &PANEL_FILTER),
    ],
};
static PANEL_SECTION_LIST: SchemaNode = SchemaNode::Array {
    item: &PANEL_SECTION,
    max_items: MAX_NODES,
};
static PANEL_SECTIONS: SchemaNode = SchemaNode::Object(&[
    SchemaProperty::required("version", &INT),
    SchemaProperty::required("view_types", &PANEL_VIEW_TYPES),
    SchemaProperty::required("sections", &PANEL_SECTION_LIST),
    SchemaProperty::optional("notice", &LABEL),
]);
static SEARCH_PANEL: SchemaNode = SchemaNode::TaggedUnion {
    discriminator: "kind",
    variants: &[
        SchemaVariant::new("selection", &PANEL_SELECTION),
        SchemaVariant::new("sections", &PANEL_SECTIONS),
        SchemaVariant::new("unsupported", &PANEL_UNSUPPORTED),
    ],
};

// ---- node kinds --------------------------------------------------------------------------------

static CHILDREN_ONLY: SchemaNode =
    SchemaNode::Object(&[SchemaProperty::required("children", &CHILDREN)]);
static FIELD_NODE: SchemaNode = SchemaNode::Object(&[
    SchemaProperty::required("field", &FIELD_REF),
    SchemaProperty::required("widget", &SHORT),
    SchemaProperty::required("options", &FIELD_OPTIONS),
    SchemaProperty::required("modifiers", &MODIFIERS),
    SchemaProperty::required("label", &LABEL),
    SchemaProperty::required("force_save", &BOOL),
]);
static LIST_NODE: SchemaNode = SchemaNode::Object(&[
    SchemaProperty::required("children", &CHILDREN),
    SchemaProperty::optional("search_panel", &SEARCH_PANEL),
    SchemaProperty::optional("page_limit", &INT),
    SchemaProperty::optional("create", &BOOL),
    SchemaProperty::optional("edit", &BOOL),
    SchemaProperty::optional("delete", &BOOL),
]);
static FORM_NODE: SchemaNode = SchemaNode::Object(&[
    SchemaProperty::required("record_mode", &SHORT),
    SchemaProperty::required("children", &CHILDREN),
    SchemaProperty::optional("create", &BOOL),
    SchemaProperty::optional("edit", &BOOL),
    SchemaProperty::optional("delete", &BOOL),
]);
static HEADING_NODE: SchemaNode = SchemaNode::Object(&[
    SchemaProperty::required("level", &INT),
    SchemaProperty::required("children", &CHILDREN),
]);
static CLASS_TOKEN: SchemaNode = SchemaNode::Text { max_bytes: 64 };
static BUTTON_CLASSES: SchemaNode = SchemaNode::Array {
    item: &CLASS_TOKEN,
    max_items: 8,
};
static BUTTON_DETAILS: SchemaNode = SchemaNode::Object(&[
    SchemaProperty::required("size", &SHORT),
    SchemaProperty::required("circle", &BOOL),
    SchemaProperty::required("classes", &BUTTON_CLASSES),
    SchemaProperty::required("debounce_ms", &INT),
    SchemaProperty::required("block_ui", &BOOL),
    SchemaProperty::optional("icon", &SHORT),
    SchemaProperty::optional("title", &LABEL),
    SchemaProperty::optional("help", &LABEL),
    SchemaProperty::optional("hotkey", &SHORT),
    SchemaProperty::optional("confirm_title", &LABEL),
    SchemaProperty::optional("confirm_label", &LABEL),
    SchemaProperty::optional("cancel_label", &LABEL),
    SchemaProperty::optional("context", &CONTEXT_EXPR),
]);
static ACTION_TARGET: SchemaNode = SchemaNode::Object(&[SchemaProperty::required("id", &INT)]);
static URL_TARGET: SchemaNode = SchemaNode::Object(&[SchemaProperty::required("href", &LABEL)]);
static INERT_TARGET: SchemaNode = SchemaNode::Object(&[]);
static BUTTON_TARGET: SchemaNode = SchemaNode::TaggedUnion {
    discriminator: "kind",
    variants: &[
        SchemaVariant::new("action", &ACTION_TARGET),
        SchemaVariant::new("url", &URL_TARGET),
        SchemaVariant::new("inert", &INERT_TARGET),
    ],
};
static ACTION_BUTTON_NODE: SchemaNode = SchemaNode::Object(&[
    SchemaProperty::required("target", &BUTTON_TARGET),
    SchemaProperty::required("label", &LABEL),
    SchemaProperty::required("variant", &SHORT),
    SchemaProperty::optional("confirm", &LABEL),
    SchemaProperty::required("modifiers", &MODIFIERS),
    SchemaProperty::optional("details", &BUTTON_DETAILS),
]);
static RIBBON_NODE: SchemaNode = SchemaNode::Object(&[
    SchemaProperty::required("text", &LABEL),
    SchemaProperty::optional("tooltip", &LABEL),
    SchemaProperty::required("background", &SHORT),
]);
static STAT_BUTTON_NODE: SchemaNode = SchemaNode::Object(&[
    SchemaProperty::required("name", &SHORT),
    SchemaProperty::required("button_type", &SHORT),
    SchemaProperty::required("label", &LABEL),
    SchemaProperty::required("variant", &SHORT),
    SchemaProperty::optional("confirm", &LABEL),
    SchemaProperty::required("modifiers", &MODIFIERS),
    SchemaProperty::optional("details", &BUTTON_DETAILS),
    SchemaProperty::required("children", &CHILDREN),
]);
static CANCEL_BUTTON_NODE: SchemaNode = SchemaNode::Object(&[
    SchemaProperty::required("label", &LABEL),
    SchemaProperty::required("variant", &SHORT),
    SchemaProperty::required("modifiers", &MODIFIERS),
    SchemaProperty::optional("details", &BUTTON_DETAILS),
]);
static BUTTON_NODE: SchemaNode = SchemaNode::Object(&[
    SchemaProperty::required("name", &SHORT),
    SchemaProperty::required("button_type", &SHORT),
    SchemaProperty::required("label", &LABEL),
    SchemaProperty::required("variant", &SHORT),
    SchemaProperty::optional("confirm", &LABEL),
    SchemaProperty::required("modifiers", &MODIFIERS),
    SchemaProperty::optional("details", &BUTTON_DETAILS),
]);
static GROUP_NODE: SchemaNode = SchemaNode::Object(&[
    SchemaProperty::required("label", &LABEL),
    SchemaProperty::required("columns", &INT),
    SchemaProperty::required("colspan", &INT),
    SchemaProperty::required("children", &CHILDREN),
]);
static PAGE_NODE: SchemaNode = SchemaNode::Object(&[
    SchemaProperty::required("name", &SHORT),
    SchemaProperty::required("label", &LABEL),
    SchemaProperty::required("children", &CHILDREN),
]);
static SEPARATOR_NODE: SchemaNode =
    SchemaNode::Object(&[SchemaProperty::required("label", &LABEL)]);
static WIDGET_NODE: SchemaNode = SchemaNode::Object(&[
    SchemaProperty::required("widget", &SHORT),
    SchemaProperty::required("options", &WIDGET_OPTIONS),
    SchemaProperty::required("modifiers", &MODIFIERS),
]);
static SEARCH_NODE: SchemaNode = SchemaNode::Object(&[
    SchemaProperty::required("children", &CHILDREN),
    SchemaProperty::optional("search_panel", &SEARCH_PANEL),
]);
static SEARCH_FIELD_NODE: SchemaNode = SchemaNode::Object(&[
    SchemaProperty::required("field", &FIELD_REF),
    SchemaProperty::required("label", &LABEL),
    SchemaProperty::optional("filter_domain", &DOMAIN_EXPR),
    SchemaProperty::optional("operator", &SHORT),
]);
static SEARCH_FILTER_NODE: SchemaNode = SchemaNode::Object(&[
    SchemaProperty::required("name", &SHORT),
    SchemaProperty::required("label", &LABEL),
    SchemaProperty::optional("domain", &DOMAIN_EXPR),
    SchemaProperty::optional("context", &CONTEXT_EXPR),
]);
static PIVOT_NODE: SchemaNode = SchemaNode::Object(&[
    SchemaProperty::required("children", &CHILDREN),
    SchemaProperty::required("disable_linking", &BOOL),
    SchemaProperty::required("display_quantity", &BOOL),
    SchemaProperty::optional("default_order", &SHORT),
]);
static PIVOT_FIELD_NODE: SchemaNode = SchemaNode::Object(&[
    SchemaProperty::required("field", &FIELD_REF),
    SchemaProperty::required("label", &LABEL),
    SchemaProperty::required("role", &SHORT),
    SchemaProperty::optional("interval", &SHORT),
]);
static EMPTY_NODE: SchemaNode = SchemaNode::Object(&[]);
static CHATTER_NODE: SchemaNode = SchemaNode::Object(&[
    SchemaProperty::required("reload_on_post", &BOOL),
    SchemaProperty::required("reload_on_attachment", &BOOL),
    SchemaProperty::required("reload_on_activity", &BOOL),
    SchemaProperty::required("reload_on_follower", &BOOL),
    SchemaProperty::required("open_attachments", &BOOL),
    SchemaProperty::required("has_activities", &BOOL),
]);
static SEARCH_GROUP_NODE: SchemaNode = SchemaNode::Object(&[
    SchemaProperty::required("label", &LABEL),
    SchemaProperty::optional("expand", &BOOL),
    SchemaProperty::required("children", &CHILDREN),
]);

static DASH_COUNT: SchemaNode = SchemaNode::Object(&[
    SchemaProperty::required("name", &SHORT),
    SchemaProperty::required("label", &LABEL),
    SchemaProperty::optional("domain", &DOMAIN_EXPR),
    SchemaProperty::optional("filter", &SHORT),
    SchemaProperty::optional("color", &SHORT),
]);
static DASH_KPI: SchemaNode = SchemaNode::Object(&[
    SchemaProperty::required("name", &SHORT),
    SchemaProperty::required("label", &LABEL),
    SchemaProperty::required("field", &FIELD_REF),
    SchemaProperty::required("date_field", &FIELD_REF),
    SchemaProperty::required("aggregate", &SHORT),
    SchemaProperty::optional("domain", &DOMAIN_EXPR),
]);
static DASH_ITEM: SchemaNode = SchemaNode::TaggedUnion {
    discriminator: "kind",
    variants: &[
        SchemaVariant::new("count", &DASH_COUNT),
        SchemaVariant::new("kpi", &DASH_KPI),
    ],
};
static DASH_ITEMS: SchemaNode = SchemaNode::Array {
    item: &DASH_ITEM,
    max_items: 32,
};
static DASHBOARD: SchemaNode = SchemaNode::Object(&[
    SchemaProperty::required("version", &INT),
    SchemaProperty::required("revision", &SHORT),
    SchemaProperty::required("items", &DASH_ITEMS),
]);

// ---- per-view-type arenas ----------------------------------------------------------------------

static LIST_UNION: SchemaNode = SchemaNode::TaggedUnion {
    discriminator: "kind",
    variants: &[
        SchemaVariant::new("list", &LIST_NODE),
        SchemaVariant::new("dashboard", &DASHBOARD),
        SchemaVariant::new("field", &FIELD_NODE),
    ],
};
static FORM_UNION: SchemaNode = SchemaNode::TaggedUnion {
    discriminator: "kind",
    variants: &[
        SchemaVariant::new("form", &FORM_NODE),
        SchemaVariant::new("sheet", &CHILDREN_ONLY),
        SchemaVariant::new("title", &CHILDREN_ONLY),
        SchemaVariant::new("heading", &HEADING_NODE),
        SchemaVariant::new("aside", &CHILDREN_ONLY),
        SchemaVariant::new("chatter", &CHATTER_NODE),
        SchemaVariant::new("header", &CHILDREN_ONLY),
        SchemaVariant::new("button_box", &CHILDREN_ONLY),
        SchemaVariant::new("stat_button", &STAT_BUTTON_NODE),
        SchemaVariant::new("footer", &CHILDREN_ONLY),
        SchemaVariant::new("cancel_button", &CANCEL_BUTTON_NODE),
        SchemaVariant::new("button", &BUTTON_NODE),
        SchemaVariant::new("action_button", &ACTION_BUTTON_NODE),
        SchemaVariant::new("ribbon", &RIBBON_NODE),
        SchemaVariant::new("notification_alert", &EMPTY_NODE),
        SchemaVariant::new("group", &GROUP_NODE),
        SchemaVariant::new("notebook", &CHILDREN_ONLY),
        SchemaVariant::new("page", &PAGE_NODE),
        SchemaVariant::new("separator", &SEPARATOR_NODE),
        SchemaVariant::new("field", &FIELD_NODE),
        SchemaVariant::new("widget", &WIDGET_NODE),
    ],
};
static SEARCH_UNION: SchemaNode = SchemaNode::TaggedUnion {
    discriminator: "kind",
    variants: &[
        SchemaVariant::new("search", &SEARCH_NODE),
        SchemaVariant::new("search_field", &SEARCH_FIELD_NODE),
        SchemaVariant::new("search_filter", &SEARCH_FILTER_NODE),
        SchemaVariant::new("search_separator", &EMPTY_NODE),
        SchemaVariant::new("search_group", &SEARCH_GROUP_NODE),
    ],
};

static PIVOT_UNION: SchemaNode = SchemaNode::TaggedUnion {
    discriminator: "kind",
    variants: &[
        SchemaVariant::new("pivot", &PIVOT_NODE),
        SchemaVariant::new("pivot_field", &PIVOT_FIELD_NODE),
    ],
};

static LIST_NODES: SchemaNode = SchemaNode::Array {
    item: &LIST_UNION,
    max_items: MAX_NODES,
};
static FORM_NODES: SchemaNode = SchemaNode::Array {
    item: &FORM_UNION,
    max_items: MAX_NODES,
};
static SEARCH_NODES: SchemaNode = SchemaNode::Array {
    item: &SEARCH_UNION,
    max_items: MAX_NODES,
};
static PIVOT_NODES: SchemaNode = SchemaNode::Array {
    item: &PIVOT_UNION,
    max_items: MAX_NODES,
};
static PIVOT_ROOT: SchemaNode = SchemaNode::Object(&[
    SchemaProperty::required("root", &INT),
    SchemaProperty::required("nodes", &PIVOT_NODES),
]);
static LIST_ROOT: SchemaNode = SchemaNode::Object(&[
    SchemaProperty::required("root", &INT),
    SchemaProperty::required("nodes", &LIST_NODES),
]);
static FORM_ROOT: SchemaNode = SchemaNode::Object(&[
    SchemaProperty::required("root", &INT),
    SchemaProperty::required("nodes", &FORM_NODES),
]);
static SEARCH_ROOT: SchemaNode = SchemaNode::Object(&[
    SchemaProperty::required("root", &INT),
    SchemaProperty::required("nodes", &SEARCH_NODES),
]);

/// The closed description schema of the builtin `list` view type.
pub fn list_schema() -> ViewResult<ViewSchema> {
    ViewSchema::new(REVISION, &LIST_ROOT)
}
/// The closed description schema of the builtin `form` view type.
pub fn form_schema() -> ViewResult<ViewSchema> {
    ViewSchema::new(REVISION, &FORM_ROOT)
}
/// The closed description schema of the builtin `search` view type.
pub fn search_schema() -> ViewResult<ViewSchema> {
    ViewSchema::new(REVISION, &SEARCH_ROOT)
}
/// The closed description schema of the builtin `pivot` view type.
pub fn pivot_schema() -> ViewResult<ViewSchema> {
    ViewSchema::new(REVISION, &PIVOT_ROOT)
}

/// Borrow a builtin's static root without constructing or validating a schema allocation.
pub(crate) fn builtin_root(kind: crate::wire::ViewType) -> Option<&'static SchemaNode> {
    match kind {
        crate::wire::ViewType::List => Some(&LIST_ROOT),
        crate::wire::ViewType::Form => Some(&FORM_ROOT),
        crate::wire::ViewType::Search => Some(&SEARCH_ROOT),
        crate::wire::ViewType::Pivot => Some(&PIVOT_ROOT),
        _ => None,
    }
}
