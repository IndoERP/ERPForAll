//! Raw, strictly spelled human UI transport DTOs.
//!
//! Deserialization is not authorization or graph admission. The metadata producer must validate
//! references, registered widgets, expression capability and caller access before publication.
//! Clients independently admit the bounded graph. These DTOs never evaluate expressions.
pub use erp_expr::wire::WireDocument;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

macro_rules! object {
    ($(#[$meta:meta])* $name:ident { $($(#[$field_meta:meta])* $field:ident : $kind:ty),* $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Serialize, Deserialize)]
        #[cfg_attr(feature="schema-gen", erp_macros::wire_ts)]
        #[serde(deny_unknown_fields)]
        #[non_exhaustive]
        pub struct $name {
            $( #[doc = concat!("Wire member `", stringify!($field), "`.")]
               $(#[$field_meta])* pub $field: $kind, )*
        }
    }
}
macro_rules! choice {
    ($name:ident { $($variant:ident),* $(,)? }) => {
        #[doc = concat!("Closed wire vocabulary for ", stringify!($name), ".")]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        #[cfg_attr(feature="schema-gen", erp_macros::wire_ts)]
        #[serde(rename_all="snake_case")]
        #[non_exhaustive]
        pub enum $name { $(#[doc = stringify!($variant)] $variant,)* }
    }
}
#[path = "wire_panel.rs"]
mod panel;
#[cfg(feature = "host-access")]
pub use panel::PanelCallerDefaults;
pub use panel::{
    PanelDefaults, PanelDisplayLimit, PanelDomain, PanelSection, PanelValue, PanelVersion,
};

object!(/// A restricted card compiled from this caller's checked architecture.
    RestrictedCard { arch: String, source_sha256: String, language: String, plan: String });

impl RestrictedCard {
    /// Bind the guest's plan to its independently computed identity.
    pub fn new(arch: String, source_sha256: String, language: String, plan: String) -> Self {
        Self {
            arch,
            source_sha256,
            language,
            plan,
        }
    }
}

/// Positive database identity representable exactly by the JSON browser client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-gen", erp_macros::wire_ts)]
#[serde(try_from = "u64", into = "u64")]
pub struct WireId(#[cfg_attr(feature = "schema-gen", ts(type = "number"))] u64);
impl TryFrom<u64> for WireId {
    type Error = &'static str;
    fn try_from(value: u64) -> Result<Self, Self::Error> {
        if (1..=9_007_199_254_740_991).contains(&value) {
            Ok(Self(value))
        } else {
            Err("identity must be a positive JSON-safe integer")
        }
    }
}
impl From<WireId> for u64 {
    fn from(value: WireId) -> Self {
        value.0
    }
}
/// Positive page size that the JSON browser client can represent exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-gen", erp_macros::wire_ts)]
#[serde(try_from = "u64", into = "u64")]
pub struct WirePageLimit(#[cfg_attr(feature = "schema-gen", ts(type = "number"))] u64);
impl TryFrom<u64> for WirePageLimit {
    type Error = &'static str;
    fn try_from(value: u64) -> Result<Self, Self::Error> {
        if (1..=9_007_199_254_740_991).contains(&value) {
            Ok(Self(value))
        } else {
            Err("page limit must be a positive JSON-safe integer")
        }
    }
}
impl From<WirePageLimit> for u64 {
    fn from(value: WirePageLimit) -> Self {
        value.0
    }
}
/// Supported wire major; unknown versions cannot deserialize as version one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-gen", erp_macros::wire_ts)]
#[serde(try_from = "u8", into = "u8")]
pub struct WireMajor(#[cfg_attr(feature = "schema-gen", ts(type = "1"))] u8);
impl WireMajor {
    /// The currently supported transport major.
    pub const V1: Self = Self(1);
}
impl TryFrom<u8> for WireMajor {
    type Error = &'static str;
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        if value == 1 {
            Ok(Self::V1)
        } else {
            Err("unsupported human UI wire major")
        }
    }
}
impl From<WireMajor> for u8 {
    fn from(value: WireMajor) -> Self {
        value.0
    }
}
object!(/// Signed anonymous login-page data, embedded as escaped JSON.
    LoginPage { csrf: String, database: String, redirect: String, #[serde(deserialize_with="required_nullable")] error: Option<String>,
        #[serde(default, deserialize_with="present_optional", skip_serializing_if = "Option::is_none")] #[cfg_attr(feature="schema-gen", ts(optional))] external_label: Option<String>,
        #[serde(default, deserialize_with="present_optional", skip_serializing_if = "Option::is_none")] #[cfg_attr(feature="schema-gen", ts(optional))] can_link_external: Option<bool> });
object!(/// Authenticated, authorized no-store shell bootstrap.
    Bootstrap { wire_major: WireMajor, registry_version: String, asset_version: String,
        identity: Identity, companies: Vec<Company>, current_company_id: WireId,
        allowed_company_ids: Vec<WireId>, active_company_ids: Vec<WireId>, locale: Locale,
        csrf: Csrf, renderers: Vec<Renderer>,
        #[serde(default)] attachment_limits: Option<AttachmentUploadLimits>,
        #[serde(default)] rpc_body_bytes: Option<u32>,
        #[serde(default)] thread_reconnect_ms: Option<[u32; 2]> });
object!(/// Effective upload policy; absent runtime is represented by a null bootstrap member.
    AttachmentUploadLimits { max_file_bytes: u32, inline_max_bytes: u32 });
impl AttachmentUploadLimits {
    /// Construct bounded limits for policy publication.
    ///
    /// # Errors
    /// Refuses a zero/overflowing file maximum or an inline maximum above the file or storage bound.
    ///
    /// ```
    /// let policy = erp_views::wire::AttachmentUploadLimits::new(1024, 512).unwrap();
    /// assert_eq!(policy.max_file_bytes, 1024);
    /// ```
    pub fn new(max: u64, inline: u64) -> Result<Self, &'static str> {
        if max == 0 || max > i32::MAX as u64 || inline > max || inline > 65536 {
            return Err("Invalid attachment upload limits.");
        }
        Ok(Self {
            max_file_bytes: max as u32,
            inline_max_bytes: inline as u32,
        })
    }
}
object!(/// Authenticated identity, never credentials.
    Identity { uid: WireId, name: String, internal: bool });
object!(/// Authorized company label.
    Company { id: WireId, name: String,
        #[serde(deserialize_with="required_nullable")] parent_id: Option<WireId> });
object!(/// Authoritative user locale.
    Locale { lang: String, tz: String,
        #[serde(default, skip_serializing_if = "Option::is_none")] direction: Option<TextDirection>,
        decimal_point: String, thousands_sep: String,
        grouping: Vec<u8>, date_format: String, time_format: String });
choice!(TextDirection { Ltr, Rtl });
object!(/// Purpose-bound form tokens.
    Csrf { logout: String });
object!(/// Registered renderer schema capability.
    Renderer { key: String, schema_major: WireMajor });
object!(/// Ordered authorized menu forest.
    MenuTree { wire_major: WireMajor, registry_version: String, roots: Vec<WireId>, nodes: Vec<MenuNode> });
object!(/// Menu node; its action remains separately authorized.
    MenuNode { id: WireId, #[serde(deserialize_with="required_nullable")] parent_id: Option<WireId>, children: Vec<WireId>, label: String,
        #[serde(deserialize_with="required_nullable")] icon: Option<String>, #[serde(deserialize_with="required_nullable")] action_id: Option<WireId>,
        /// Which action model `action_id` names; null when the menu has no action. Ids are unique across action models.
        #[serde(deserialize_with="required_nullable")] action_kind: Option<MenuActionKind>,
        /// Nonempty namespace of the oldest menu external ID, or null when absent/empty; never a permission grant.
        #[serde(deserialize_with="required_nullable")] module: Option<String> });
choice!(MenuActionKind { Window, Client });
choice!(ActionTarget { Current, Main, New });
choice!(ViewType {
    List,
    Form,
    Search,
    Kanban,
    Calendar,
    Pivot,
    Graph,
    Gantt,
    Activity,
    Map,
    Hierarchy
});
impl ViewType {
    /// Wire name of the view type.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Form => "form",
            Self::Search => "search",
            Self::Kanban => "kanban",
            Self::Calendar => "calendar",
            Self::Pivot => "pivot",
            Self::Graph => "graph",
            Self::Gantt => "gantt",
            Self::Activity => "activity",
            Self::Map => "map",
            Self::Hierarchy => "hierarchy",
        }
    }

    /// Parse a ViewType from its wire name.
    pub fn from_wire_str(s: &str) -> Option<Self> {
        match s {
            "list" => Some(Self::List),
            "form" => Some(Self::Form),
            "search" => Some(Self::Search),
            "kanban" => Some(Self::Kanban),
            "calendar" => Some(Self::Calendar),
            "pivot" => Some(Self::Pivot),
            "graph" => Some(Self::Graph),
            "gantt" => Some(Self::Gantt),
            "activity" => Some(Self::Activity),
            "map" => Some(Self::Map),
            "hierarchy" => Some(Self::Hierarchy),
            _ => None,
        }
    }
}
object!(/// Authorized window action. Unsupported registered view types fail at admission.
    WindowAction { wire_major: WireMajor, id: WireId, name: String, model: String,
        target: ActionTarget, views: Vec<ActionView>, #[serde(deserialize_with="required_nullable")] domain: Option<WireDocument>, #[serde(deserialize_with="required_nullable")] context: Option<WireDocument> });
object!(/// Selected view request attached to an action.
    ActionView { #[serde(deserialize_with="required_nullable")] id: Option<WireId>, #[serde(rename="type")] view_type: ViewType });
object!(/// Typed accessible view descriptions for one model.
    ViewDescription { wire_major: WireMajor, registry_version: String, description_version: String,
        model: String, fields: BTreeMap<String, BTreeMap<String, FieldDescription>>,
        views: Vec<View>, notices: Vec<Notice> });
object!(/// Caller-authorized action and its view descriptions, produced atomically.
    WindowActionViews { action: WindowAction, views: ViewDescription });
choice!(FieldKind {
    Char,
    Text,
    Html,
    Boolean,
    Integer,
    Float,
    Monetary,
    Date,
    Datetime,
    Selection,
    Many2one,
    One2many,
    Many2many,
    Binary,
    Json
});
object!(/// Accessible model field metadata; field types never silently fall back.
    FieldDescription { #[serde(rename="type")] field_type: FieldKind, label: String,
        required: bool, readonly: bool, #[serde(deserialize_with="required_nullable")] selection: Option<Vec<(String,String)>>, #[serde(deserialize_with="required_nullable")] relation: Option<String>,
        #[serde(deserialize_with="required_nullable")] relation_field: Option<String> });
/// A selected persisted view or a genuinely generated view.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-gen", erp_macros::wire_ts)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum ViewIdentity {
    /// Real persisted record identity.
    Stored {
        /// Positive identity.
        id: WireId,
    },
    /// Generated fallback, never a fake record zero.
    Generated {},
}
object!(/// Registered arena representation; root and every child require bounds checks.
    View { identity: ViewIdentity, #[serde(rename="type")] view_type: ViewType,
        schema_major: WireMajor, renderer: String, root: u32, nodes: Vec<ViewNode> });
object!(/// Accessible field handle.
    FieldRef { model: String, name: String });
object!(/// Optional validated expression documents, never evaluation strings.
    Modifiers { #[serde(deserialize_with="required_nullable")] readonly: Option<WireDocument>, #[serde(deserialize_with="required_nullable")] required: Option<WireDocument>, #[serde(deserialize_with="required_nullable")] invisible: Option<WireDocument> });
object!(/// Empty options for the currently supported basic fields and Addons service.
    EmptyOptions {});
choice!(RecordMode { Record, Service });
choice!(EditMode { Read, Edit });
choice!(PivotRole { Row, Col, Measure });
choice!(KanbanRegion {
    Main,
    Aside,
    Footer
});
choice!(ProgressTone {
    Success,
    Warning,
    Danger,
    Info,
    Muted
});
object!(/// One coloured value of a kanban progress bar.
    ProgressColor { value: String, tone: ProgressTone });
choice!(CalendarScale {
    Day,
    Week,
    Month,
    Year
});
choice!(GraphMode { Bar, Line, Pie });
choice!(GraphRole { Group, Measure });
choice!(GraphOrder { Asc, Desc });
choice!(ButtonType { Object });
choice!(ButtonVariant {
    Primary,
    Secondary,
    Link,
    Success,
    Info,
    Warning,
    Danger,
    Unstyled
});
choice!(ButtonSize {
    Small,
    Normal,
    Large
});
choice!(RibbonBackground {
    Info,
    Success,
    Warning,
    Danger
});
choice!(PromptKind {
    Text,
    Date,
    Selection
});
/// A selection prompt choice value: 1 to 64 ASCII lowercase letters, digits or `_`, starting with a letter.
pub fn valid_prompt_choice_value(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.starts_with(|c: char| c.is_ascii_lowercase())
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}
object!(/// One choice of a selection prompt: the value sent and its caption.
    PromptChoice { value: String, label: String });
object!(/// A short answer the client asks for before an object button calls its method. The answer is the method's
/// `prompt_value` keyword argument: text, an ISO date or a choice value, and `null` when an optional answer is empty.
ButtonPrompt {
    kind: PromptKind, label: String, required: bool,
    #[serde(default, deserialize_with="present_optional", skip_serializing_if="Option::is_none")] #[cfg_attr(feature="schema-gen", ts(optional))] max_length: Option<u32>,
    #[serde(default, deserialize_with="present_optional", skip_serializing_if="Option::is_none")] #[cfg_attr(feature="schema-gen", ts(optional))] choices: Option<Vec<PromptChoice>>
});
object!(/// Optional, bounded button presentation and execution declarations.
ButtonDetails {
    size: ButtonSize, circle: bool, classes: Vec<String>, debounce_ms: u32, block_ui: bool,
    #[serde(default, deserialize_with="present_optional", skip_serializing_if="Option::is_none")] #[cfg_attr(feature="schema-gen", ts(optional))] icon: Option<String>,
    #[serde(default, deserialize_with="present_optional", skip_serializing_if="Option::is_none")] #[cfg_attr(feature="schema-gen", ts(optional))] title: Option<String>,
    #[serde(default, deserialize_with="present_optional", skip_serializing_if="Option::is_none")] #[cfg_attr(feature="schema-gen", ts(optional))] help: Option<String>,
    #[serde(default, deserialize_with="present_optional", skip_serializing_if="Option::is_none")] #[cfg_attr(feature="schema-gen", ts(optional))] hotkey: Option<String>,
    #[serde(default, deserialize_with="present_optional", skip_serializing_if="Option::is_none")] #[cfg_attr(feature="schema-gen", ts(optional))] confirm_title: Option<String>,
    #[serde(default, deserialize_with="present_optional", skip_serializing_if="Option::is_none")] #[cfg_attr(feature="schema-gen", ts(optional))] confirm_label: Option<String>,
    #[serde(default, deserialize_with="present_optional", skip_serializing_if="Option::is_none")] #[cfg_attr(feature="schema-gen", ts(optional))] cancel_label: Option<String>,
    #[serde(default, deserialize_with="present_optional", skip_serializing_if="Option::is_none")] #[cfg_attr(feature="schema-gen", ts(optional))] context: Option<WireDocument>,
    #[serde(default, deserialize_with="present_optional", skip_serializing_if="Option::is_none")] #[cfg_attr(feature="schema-gen", ts(optional))] prompt: Option<ButtonPrompt>
});
/// Explicit non-object execution identity; captions never select an action.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-gen", erp_macros::wire_ts)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum ButtonTarget {
    /// Load the declared action identity.
    Action {
        /// Positive JSON-safe action identity.
        id: WireId,
    },
    /// Navigate to a separately validated HTTP(S) URL or same-origin route.
    Url {
        /// URL text; admission precedes side effects.
        href: String,
    },
    /// A declaration with no action.
    Inert,
}

/// Whether a class token belongs to the bounded addon CSS identifier vocabulary.
pub fn valid_button_class_token(token: &str) -> bool {
    let mut bytes = token.bytes();
    token.len() <= 64
        && bytes
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
/// Whether a declared hotkey uses the lowercase access-key vocabulary.
pub fn valid_button_hotkey(key: &str) -> bool {
    let key = key.strip_prefix("shift+").unwrap_or(key);
    key.len() == 1
        && key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}
/// Admit HTTP(S) URLs or absolute same-origin routes; refuse every other scheme and control text.
pub fn valid_button_url(href: &str) -> bool {
    if href.is_empty()
        || href.len() > 4096
        || href
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
    {
        return false;
    }
    if href.starts_with('/') {
        return !href.starts_with("//");
    }
    let Some((scheme, rest)) = href.split_once("://") else {
        return false;
    };
    (scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
        && !rest.split(['/', '?', '#']).next().unwrap_or("").is_empty()
}
choice!(ScopeKind { Global, Company });
object!(/// Settings service presentation: scopes (not a permission grant) and `app`, the addon whose settings page the
    /// form opens on (its Configuration > Settings menu); `None` opens the first app.
    SettingsOptions { scope_kinds: Vec<ScopeKind>, #[serde(deserialize_with="required_nullable")] app: Option<String> });
impl SettingsOptions {
    /// Construct the presentation scopes declared by a validated service form; it opens on the first app.
    pub fn new(scope_kinds: Vec<ScopeKind>) -> Self {
        Self {
            scope_kinds,
            app: None,
        }
    }
    /// Open on one addon's settings page. The name must pass [`is_settings_app_name`]; callers validate it first.
    pub fn with_app(mut self, app: String) -> Self {
        self.app = Some(app);
        self
    }
}
/// An addon technical name as a settings page key: lowercase ASCII letter first, then lowercase letters, digits or
/// `_`, at most 64 bytes. Setting keys group by this prefix (`<addon>.<name>`), so anything else cannot match a page.
pub fn is_settings_app_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    name.len() <= 64
        && bytes.next().is_some_and(|b| b.is_ascii_lowercase())
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}
/// Strict service-widget options; widget registration validates the corresponding shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-gen", erp_macros::wire_ts)]
#[serde(untagged)]
#[non_exhaustive]
pub enum WidgetOptions {
    /// Settings scope presentation.
    Settings(SettingsOptions),
    /// Addons has no options.
    Empty(EmptyOptions),
}
object!(/// Selection statusbar presentation; `null` shows all choices, otherwise the current choice is retained alongside these keys.
    StatusbarOptions { #[serde(deserialize_with="required_nullable")] visible: Option<Vec<String>> });
impl StatusbarOptions {
    /// Construct already-admitted selection visibility. XML admission validates keys against the field.
    pub fn new(visible: Option<Vec<String>>) -> Self {
        Self { visible }
    }
}
object!(/// Closed optional presentation members; widget identity admission restricts each family.
    FieldPresentationOptions {
        #[serde(default, skip_serializing_if="Option::is_none")] help: Option<String>,
        #[serde(default, skip_serializing_if="Option::is_none")] placeholder: Option<String>,
        #[serde(default, skip_serializing_if="Option::is_none")] autocomplete: Option<String>,
        #[serde(default, skip_serializing_if="Option::is_none")] icon: Option<String>,
        #[serde(default, skip_serializing_if="Option::is_none")] icon_class: Option<String>,
        #[serde(default, skip_serializing_if="Option::is_none")] alt: Option<String>,
        #[serde(default, skip_serializing_if="Option::is_none")] img_class: Option<String>,
        #[serde(default, skip_serializing_if="Option::is_none")] accepted_file_extensions: Option<String>,
        #[serde(default, skip_serializing_if="Option::is_none")] min_date: Option<String>,
        #[serde(default, skip_serializing_if="Option::is_none")] max_date: Option<String>,
        #[serde(default, skip_serializing_if="Option::is_none")] min_precision: Option<String>,
        #[serde(default, skip_serializing_if="Option::is_none")] max_precision: Option<String>,
        #[serde(default, skip_serializing_if="Option::is_none")] warn: Option<String>,
        #[serde(default, skip_serializing_if="Option::is_none")] r#type: Option<String>,
        #[serde(default, skip_serializing_if="Option::is_none")] text: Option<String>,
        #[serde(default, skip_serializing_if="Option::is_none")] password: Option<bool>,
        #[serde(default, skip_serializing_if="Option::is_none")] nolabel: Option<bool>,
        #[serde(default, skip_serializing_if="Option::is_none")] no_open: Option<bool>,
        #[serde(default, skip_serializing_if="Option::is_none")] no_create: Option<bool>,
        #[serde(default, skip_serializing_if="Option::is_none")] no_quick_create: Option<bool>,
        #[serde(default, skip_serializing_if="Option::is_none")] no_create_edit: Option<bool>,
        #[serde(default, skip_serializing_if="Option::is_none")] no_symbol: Option<bool>,
        #[serde(default, skip_serializing_if="Option::is_none")] enable_formatting: Option<bool>,
        #[serde(default, skip_serializing_if="Option::is_none")] hide_trailing_zeros: Option<bool>,
        #[serde(default, skip_serializing_if="Option::is_none")] field_digits: Option<bool>,
        #[serde(default, skip_serializing_if="Option::is_none")] show_time: Option<bool>,
        #[serde(default, skip_serializing_if="Option::is_none")] show_seconds: Option<bool>,
        #[serde(default, skip_serializing_if="Option::is_none")] always_range: Option<bool>,
        #[serde(default, skip_serializing_if="Option::is_none")] numeric: Option<bool>,
        #[serde(default, skip_serializing_if="Option::is_none")] create: Option<bool>,
        #[serde(default, skip_serializing_if="Option::is_none")] can_open: Option<bool>,
        #[serde(default, skip_serializing_if="Option::is_none")] horizontal: Option<bool>,
        #[serde(default, skip_serializing_if="Option::is_none")] zoom: Option<bool>,
        #[serde(default, skip_serializing_if="Option::is_none")] reload: Option<bool>,
        #[serde(default, skip_serializing_if="Option::is_none")] convert_to_webp: Option<bool>,
        #[serde(default, skip_serializing_if="Option::is_none")] #[cfg_attr(feature="schema-gen", ts(type="number | null"))] rows: Option<i64>,
        #[serde(default, skip_serializing_if="Option::is_none")] #[cfg_attr(feature="schema-gen", ts(type="number | null"))] max_length: Option<i64>,
        #[serde(default, skip_serializing_if="Option::is_none")] #[cfg_attr(feature="schema-gen", ts(type="number | null"))] badge_limit: Option<i64>,
        #[serde(default, skip_serializing_if="Option::is_none")] #[cfg_attr(feature="schema-gen", ts(type="number | null"))] tag_limit: Option<i64>,
        #[serde(default, skip_serializing_if="Option::is_none")] #[cfg_attr(feature="schema-gen", ts(type="number | null"))] search_threshold: Option<i64>,
        #[serde(default, skip_serializing_if="Option::is_none")] #[cfg_attr(feature="schema-gen", ts(type="number | null"))] rounding: Option<i64>,
        #[serde(default, skip_serializing_if="Option::is_none")] #[cfg_attr(feature="schema-gen", ts(type="number | null"))] zoom_delay: Option<i64>,
        #[serde(default, skip_serializing_if="Option::is_none")] placeholder_field: Option<FieldRef>,
        #[serde(default, skip_serializing_if="Option::is_none")] currency_field: Option<FieldRef>,
        #[serde(default, skip_serializing_if="Option::is_none")] start_date_field: Option<FieldRef>,
        #[serde(default, skip_serializing_if="Option::is_none")] end_date_field: Option<FieldRef>,
        #[serde(default, skip_serializing_if="Option::is_none")] whitelist_fname: Option<FieldRef>,
        #[serde(default, skip_serializing_if="Option::is_none")] color_field: Option<FieldRef>,
        #[serde(default, skip_serializing_if="Option::is_none")] preview_image: Option<FieldRef>,
        #[serde(default, skip_serializing_if="Option::is_none")] filename: Option<FieldRef>,
        #[serde(default, skip_serializing_if="Option::is_none")] whitelisted_values: Option<Vec<String>>,
        #[serde(default, skip_serializing_if="Option::is_none")] blacklisted_values: Option<Vec<String>>,
        #[serde(default, skip_serializing_if="Option::is_none")] tone_success: Option<Vec<String>>,
        #[serde(default, skip_serializing_if="Option::is_none")] tone_info: Option<Vec<String>>,
        #[serde(default, skip_serializing_if="Option::is_none")] tone_warning: Option<Vec<String>>,
        #[serde(default, skip_serializing_if="Option::is_none")] tone_danger: Option<Vec<String>>,
        #[serde(default, skip_serializing_if="Option::is_none")] tone_muted: Option<Vec<String>>,
        #[serde(default, skip_serializing_if="Option::is_none")] #[cfg_attr(feature="schema-gen", ts(type="number[] | null"))] digits: Option<Vec<i64>>,
        #[serde(default, skip_serializing_if="Option::is_none")] #[cfg_attr(feature="schema-gen", ts(type="number[] | null"))] size: Option<Vec<i64>>,
        #[serde(default, skip_serializing_if="Option::is_none")] domain: Option<WireDocument>,
        #[serde(default, skip_serializing_if="Option::is_none")] context: Option<WireDocument> });
impl FieldPresentationOptions {
    /// Empty presentation, retaining the existing empty-object spelling.
    pub fn empty() -> Self {
        Self {
            help: None,
            placeholder: None,
            autocomplete: None,
            icon: None,
            icon_class: None,
            alt: None,
            img_class: None,
            accepted_file_extensions: None,
            min_date: None,
            max_date: None,
            min_precision: None,
            max_precision: None,
            warn: None,
            r#type: None,
            text: None,
            password: None,
            nolabel: None,
            no_open: None,
            no_create: None,
            no_quick_create: None,
            no_create_edit: None,
            no_symbol: None,
            enable_formatting: None,
            hide_trailing_zeros: None,
            field_digits: None,
            show_time: None,
            show_seconds: None,
            always_range: None,
            numeric: None,
            create: None,
            can_open: None,
            horizontal: None,
            zoom: None,
            reload: None,
            convert_to_webp: None,
            rows: None,
            max_length: None,
            badge_limit: None,
            tag_limit: None,
            search_threshold: None,
            rounding: None,
            zoom_delay: None,
            placeholder_field: None,
            currency_field: None,
            start_date_field: None,
            end_date_field: None,
            whitelist_fname: None,
            color_field: None,
            preview_image: None,
            filename: None,
            whitelisted_values: None,
            blacklisted_values: None,
            tone_success: None,
            tone_info: None,
            tone_warning: None,
            tone_danger: None,
            tone_muted: None,
            digits: None,
            size: None,
            domain: None,
            context: None,
        }
    }
}
choice!(CodeMode {
    Xml,
    Python,
    Javascript,
    Json,
    Qweb,
    Scss,
    Bash,
    Text
});
impl CodeMode {
    /// The admitted presentation mode; source is never executed.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Xml => "xml",
            Self::Python => "python",
            Self::Javascript => "javascript",
            Self::Json => "json",
            Self::Qweb => "qweb",
            Self::Scss => "scss",
            Self::Bash => "bash",
            Self::Text => "text",
        }
    }
    /// Parse only the closed, case-sensitive field-editor mode vocabulary.
    pub fn from_wire_str(value: &str) -> Option<Self> {
        match value {
            "xml" => Some(Self::Xml),
            "python" => Some(Self::Python),
            "javascript" => Some(Self::Javascript),
            "json" => Some(Self::Json),
            "qweb" => Some(Self::Qweb),
            "scss" => Some(Self::Scss),
            "bash" => Some(Self::Bash),
            "text" => Some(Self::Text),
            _ => None,
        }
    }
}
object!(/// Code-field presentation with an explicit admitted mode.
    CodeOptions { mode: CodeMode });
impl CodeOptions {
    /// Construct presentation options from an already admitted mode.
    pub fn new(mode: CodeMode) -> Self {
        Self { mode }
    }
}
/// Identity-specific field options. Existing widgets preserve the exact empty-object wire shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-gen", erp_macros::wire_ts)]
#[serde(untagged)]
#[non_exhaustive]
pub enum FieldOptions {
    /// Text-only code editor presentation.
    Code(CodeOptions),
    /// Admitted builtin presentation, validated against its widget identity.
    Presentation(Box<FieldPresentationOptions>),
    /// Selection statusbar visibility.
    Statusbar(StatusbarOptions),
    /// Basic and addon widgets have no options in the current contract.
    Empty(EmptyOptions),
}
/// Registered service widget identities. Unknown component strings are refused.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-gen", erp_macros::wire_ts)]
#[non_exhaustive]
pub enum ServiceWidget {
    /// Typed settings service.
    #[serde(rename = "web.settings")]
    Settings,
    /// Native addon service.
    #[serde(rename = "web.addons")]
    Addons,
}
/// Caller-visible standard search-panel presentation for the first supported selection category.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-gen", erp_macros::wire_ts)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum ListSearchPanel {
    /// Extended descriptions, admitted only under the section-capable list/search renderer keys.
    Sections {
        /// Exact supported panel contract version.
        version: PanelVersion,
        /// Declared applicable view keys; these do not register unavailable view types.
        view_types: Vec<String>,
        /// Ordered caller-authorized sections.
        sections: Vec<PanelSection>,
        /// A bounded fixed compatibility or existing view-selection notice.
        #[serde(deserialize_with = "required_nullable")]
        notice: Option<String>,
    },
    /// One single-select selection category from the inherited, authorized search architecture.
    Selection {
        /// Accessible stored selection field.
        field: FieldRef,
        /// XML string override or the field label.
        label: String,
        /// Standard enable_counters flag.
        enable_counters: bool,
        /// Standard expand flag; false shows only values present in the current base search.
        expand: bool,
        /// Context searchpanel_default_FIELD, with false represented as no default.
        default_value: Option<String>,
        /// An explicit existing view-selection fallback notice, when present.
        notice: Option<String>,
    },
    /// An unimplemented panel remains visibly unavailable, without changing the list domain.
    Unsupported {
        /// Precise supported-scope boundary.
        message: String,
    },
}
/// Declarative dashboard metric; references are descriptive and require host authorization.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-gen", erp_macros::wire_ts)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum DashboardItem {
    /// A count narrowed by a domain or a companion search-view filter.
    Count {
        /// Stable item identity in the owning view.
        name: String,
        /// Human-readable caption from the owning view.
        label: String,
        /// Admitted root-model domain, mutually exclusive with filter.
        domain: Option<WireDocument>,
        /// Companion search-view filter name, mutually exclusive with domain.
        filter: Option<String>,
        /// Optional semantic color role.
        color: Option<String>,
    },
    /// Numeric aggregation over current and previous periods.
    Kpi {
        /// Stable item identity in the owning view.
        name: String,
        /// Human-readable caption from the owning view.
        label: String,
        /// Stored numeric measure.
        field: FieldRef,
        /// Stored date or datetime field.
        date_field: FieldRef,
        /// Admitted aggregate function.
        aggregate: String,
        /// Optional additional root-model domain.
        domain: Option<WireDocument>,
    },
}
object!(/// A list column's width from its arch `width` attribute, in content pixels (padding excluded):
    /// `max` equal to `min` is fixed, no `max` is a minimum.
    ColumnWidth {
        min: u16,
        #[serde(default, deserialize_with="present_optional", skip_serializing_if="Option::is_none")]
        #[cfg_attr(feature="schema-gen", ts(optional))] max: Option<u16> });
impl ColumnWidth {
    /// Largest width an arch may set, in pixels.
    pub const MAX_PX: u16 = 2000;
    /// Construct a bounded column width.
    ///
    /// # Errors
    /// Refuses a bound outside 1 to [`Self::MAX_PX`] or a maximum below the minimum.
    ///
    /// ```
    /// let width = erp_views::wire::ColumnWidth::new(100, Some(400)).unwrap();
    /// assert_eq!((width.min, width.max), (100, Some(400)));
    /// assert!(erp_views::wire::ColumnWidth::new(300, Some(200)).is_err());
    /// ```
    pub fn new(min: u16, max: Option<u16>) -> Result<Self, &'static str> {
        let bounded = |px: u16| (1..=Self::MAX_PX).contains(&px);
        if !bounded(min) || max.is_some_and(|max| !bounded(max) || max < min) {
            return Err("Invalid list column width.");
        }
        Ok(Self { min, max })
    }
}
/// Strict list/form arena, including the static record-form layout.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-gen", erp_macros::wire_ts)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum ViewNode {
    /// View-neutral dashboard region; initially attached to list views.
    Dashboard {
        /// Closed dashboard contract version.
        version: u32,
        /// Content revision of the saved declaration.
        revision: String,
        /// Ordered metric declarations, never list row fields.
        items: Vec<DashboardItem>,
    },
    /// List root.
    List {
        /// Child arena indexes.
        children: Vec<u32>,
        /// Authorized standard search-panel category or explicit unsupported-scope notice.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        search_panel: Option<ListSearchPanel>,
        /// Positive XML limit override or the admitted action limit; absent keeps legacy clients.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        page_limit: Option<WirePageLimit>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        create: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        edit: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        delete: Option<bool>,
    },
    /// Record or service form root.
    Form {
        /// Explicit persistence mode.
        record_mode: RecordMode,
        /// Child arena indexes.
        children: Vec<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        create: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        edit: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        delete: Option<bool>,
        /// `read`: a saved record opens read-first, with an Edit command; absent means `edit` (always editable).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        edit_mode: Option<EditMode>,
    },
    /// Record-form sheet.
    Sheet {
        /// Child arena indexes.
        children: Vec<u32>,
    },
    /// Explicit standard XML title region (`div.oe_title`), independent of model or field identity.
    Title {
        /// Heading and field arena indexes.
        children: Vec<u32>,
    },
    /// A declared title heading; admission currently supports h1 and h2 only.
    Heading {
        /// Native heading level (1 or 2).
        level: u8,
        /// Field arena indexes.
        children: Vec<u32>,
    },
    /// Explicit supplementary record-form region; direct root child, at most one.
    Aside {
        /// Authorized field/group/separator arena indexes.
        children: Vec<u32>,
    },
    /// Chatter slot of a record form; direct root child, at most one, no children (addendum).
    Chatter {
        /// Reload the parent record after a message is posted.
        reload_on_post: bool,
        /// Reload the parent record after an attachment change.
        reload_on_attachment: bool,
        /// Reload the parent record after an activity change.
        reload_on_activity: bool,
        /// Reload the parent record after a follower change.
        reload_on_follower: bool,
        /// Show the attachment panel initially.
        open_attachments: bool,
        /// The model inherits the activity mixin; set by the server from the model, never from the arch.
        has_activities: bool,
    },
    /// Record-form header bar holding object buttons and selection statusbars; first child of the form root.
    Header {
        /// Button and statusbar field arena indexes.
        children: Vec<u32>,
    },
    /// XML-declared smart-button area, kept inside the form sheet.
    ButtonBox {
        /// Smart-button arena indexes.
        children: Vec<u32>,
    },
    /// Object action with readonly statistic fields; no count is inferred by the client.
    StatButton {
        /// Exposed model method.
        name: String,
        /// Call kind, currently object only.
        button_type: ButtonType,
        /// Accessible action caption.
        label: String,
        /// Closed emphasis vocabulary.
        variant: ButtonVariant,
        /// Confirmation before calling, if declared.
        #[serde(deserialize_with = "required_nullable")]
        confirm: Option<String>,
        /// Record-dependent visibility.
        modifiers: Modifiers,
        /// Optional declarations; absence preserves the legacy payload shape.
        #[serde(
            default,
            deserialize_with = "present_optional",
            skip_serializing_if = "Option::is_none"
        )]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        details: Option<ButtonDetails>,
        /// Readonly statistic field indexes.
        children: Vec<u32>,
    },
    /// Dialog footer bar of a record form: the last direct child of the form root,
    /// holding object buttons and at most one cancel button (producer and client both enforce it).
    Footer {
        /// Button arena indexes.
        children: Vec<u32>,
    },
    /// Footer button that closes the dialog without saving or calling a method. It has no method
    /// name by construction.
    CancelButton {
        /// Caption shown on the button.
        label: String,
        /// Closed visual emphasis.
        variant: ButtonVariant,
        /// `invisible` is the only supported expression.
        modifiers: Modifiers,
        /// Optional declarations; absence preserves the legacy payload shape.
        #[serde(
            default,
            deserialize_with = "present_optional",
            skip_serializing_if = "Option::is_none"
        )]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        details: Option<ButtonDetails>,
    },
    /// Header button that calls a model method on the saved record.
    Button {
        /// Method name called as `POST /json/2/<model>/<name>`.
        name: String,
        /// Call kind; only object methods exist in this slice.
        button_type: ButtonType,
        /// Caption shown on the button.
        label: String,
        /// Closed visual emphasis.
        variant: ButtonVariant,
        /// Confirmation question asked before the call, if any.
        #[serde(deserialize_with = "required_nullable")]
        confirm: Option<String>,
        /// `invisible` is the only supported expression; it sees the record's view fields.
        modifiers: Modifiers,
        /// Optional declarations; absence preserves the legacy payload shape.
        #[serde(
            default,
            deserialize_with = "present_optional",
            skip_serializing_if = "Option::is_none"
        )]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        details: Option<ButtonDetails>,
    },
    /// Header/footer non-object action, URL or inert control.
    ActionButton {
        /// Explicit execution target.
        target: ButtonTarget,
        /// Accessible caption (or a nonempty details title).
        label: String,
        /// Presentation rank.
        variant: ButtonVariant,
        /// Optional confirmation question, with legacy required nullable spelling.
        #[serde(deserialize_with = "required_nullable")]
        confirm: Option<String>,
        /// Visibility expression only.
        modifiers: Modifiers,
        /// Optional presentation and execution details.
        #[serde(
            default,
            deserialize_with = "present_optional",
            skip_serializing_if = "Option::is_none"
        )]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        details: Option<ButtonDetails>,
    },
    /// Record-form display ribbon; no service or execution identity.
    Ribbon {
        /// Plain text caption.
        text: String,
        /// Optional plain text tooltip.
        #[serde(
            default,
            deserialize_with = "present_optional",
            skip_serializing_if = "Option::is_none"
        )]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        tooltip: Option<String>,
        /// Semantic background.
        background: RibbonBackground,
        /// Per-record visibility: the ribbon is hidden while this expression evaluates to true.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        invisible: Option<WireDocument>,
    },
    /// Browser-owned notification permission feedback, with no server permission state.
    NotificationAlert {},
    /// Labelled layout group; track counts are even values from 2 through 12.
    Group {
        /// Optional caption (empty when absent).
        label: String,
        /// Label/control tracks, two per rendered column.
        columns: u32,
        /// Requested span in label/control tracks, constrained by the client grid.
        colspan: u32,
        /// Child arena indexes.
        children: Vec<u32>,
    },
    /// Notebook whose children are exclusively pages.
    Notebook {
        /// Page arena indexes.
        children: Vec<u32>,
    },
    /// Named notebook page.
    Page {
        /// Stable page identity within its notebook.
        name: String,
        /// Display caption.
        label: String,
        /// Child arena indexes.
        children: Vec<u32>,
        /// Per-record visibility: the page and its tab are hidden while this expression is true.
        /// Absent means always shown (a static `invisible="1"` page is pruned before the wire).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        invisible: Option<WireDocument>,
    },
    /// Static section caption with no children.
    Separator {
        /// Display caption.
        label: String,
    },
    /// Registered field widget.
    Field {
        /// Accessible field handle.
        field: FieldRef,
        /// Registered widget key.
        widget: String,
        /// Closed widget-specific options.
        options: FieldOptions,
        /// Static boolean-literal overrides for this occurrence (`null` inherits model metadata;
        /// invisible `null` means false).
        modifiers: Modifiers,
        /// Display label.
        label: String,
        /// Whether a readonly value is still written on save; false when omitted.
        force_save: bool,
        /// List column width set by the arch; omitted when it sets none.
        #[serde(
            default,
            deserialize_with = "present_optional",
            skip_serializing_if = "Option::is_none"
        )]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        width: Option<ColumnWidth>,
    },
    /// Explicit service widget.
    Widget {
        /// Registered identity.
        widget: ServiceWidget,
        /// Strict identity-specific options.
        options: WidgetOptions,
        /// Expression constraints.
        modifiers: Modifiers,
    },
    /// Search view root.
    Search {
        /// Child arena indexes.
        children: Vec<u32>,
        /// Authorized panel metadata from this standalone search view.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        search_panel: Option<ListSearchPanel>,
    },
    /// Pivot root (slice 1).
    Pivot {
        /// Child arena indexes (only `pivot_field`).
        children: Vec<u32>,
        disable_linking: bool,
        display_quantity: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        default_order: Option<String>,
        /// Optional, named client extension for displaying measures.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        variant: Option<String>,
    },
    /// One `<field>` of a pivot: a row grouping, a column grouping or a measure.
    PivotField {
        /// Accessible field handle.
        field: FieldRef,
        /// Display label.
        label: String,
        /// What the field does in the table.
        role: PivotRole,
        /// Date grouping interval: day, week, month, quarter or year (date and datetime row/column fields only).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        interval: Option<String>,
    },
    /// Kanban root: one card and at most one progress bar.
    Kanban {
        /// Child arena indexes (`kanban_card`, `kanban_progressbar`).
        children: Vec<u32>,
        /// Cards per page from the arch `limit`; absent keeps the client's list default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        page_limit: Option<WirePageLimit>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        default_group_by: Option<FieldRef>,
        /// Stored many2one shared by board records and their column comodel. Selects a catalogue of columns.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        group_scope: Option<FieldRef>,
        quick_create: bool,
        records_draggable: bool,
        groups_draggable: bool,
        archivable: bool,
        can_open: bool,
        /// Whether new records can be created from this view.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        create: Option<bool>,
        /// Whether records can be edited from this view.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        edit: Option<bool>,
        /// Whether records can be deleted from this view.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        delete: Option<bool>,
    },
    /// The card of a kanban (`<t t-name="card">`): fields and regions.
    KanbanCard {
        /// Optional host-produced plan after field authorization.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        restricted: Option<RestrictedCard>,
        /// Child arena indexes (`field`, `kanban_section`).
        children: Vec<u32>,
    },
    /// A card region (`<main>`, `<aside>`, `<footer>`), holding fields only.
    KanbanSection {
        /// Which region.
        region: KanbanRegion,
        /// Child arena indexes (`field`).
        children: Vec<u32>,
    },
    /// A kanban progress bar declaration (`<progressbar>`), drawn by grouped columns.
    KanbanProgressbar {
        /// Field whose values the segments count.
        field: FieldRef,
        /// Coloured values in arch order.
        colors: Vec<ProgressColor>,
        /// Field summed per column.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        sum_field: Option<FieldRef>,
        /// Help text of the bar.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        help: Option<String>,
    },
    /// Calendar root: which fields place an event and what the grid offers.
    Calendar {
        /// Child arena indexes (only `calendar_field`).
        children: Vec<u32>,
        date_start: FieldRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        date_stop: Option<FieldRef>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        all_day: Option<FieldRef>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        color: Option<FieldRef>,
        mode: CalendarScale,
        scales: Vec<CalendarScale>,
        create: bool,
        edit: bool,
        delete: bool,
        quick_create: bool,
        hide_date: bool,
        hide_time: bool,
        month_overflow: bool,
        event_limit: u32,
    },
    /// One `<field>` of a calendar: shown in the event popover unless invisible; with `filters`, a side-panel filter.
    CalendarField {
        /// Accessible field handle.
        field: FieldRef,
        /// Display label.
        label: String,
        /// Read for the event but not shown in its popover.
        invisible: bool,
        filters: bool,
    },
    /// Graph root: the chart a graph view draws first.
    Graph {
        /// Child arena indexes (only `graph_field`).
        children: Vec<u32>,
        mode: GraphMode,
        stacked: bool,
        cumulated: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        order: Option<GraphOrder>,
        disable_linking: bool,
    },
    /// Hierarchy (org chart / tree) root.
    Hierarchy {
        /// Child arena indexes (card `field` nodes).
        children: Vec<u32>,
        /// Self-referential parent `many2one` field.
        parent_field: FieldRef,
        /// Optional inverse `one2many` children field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        child_field: Option<FieldRef>,
        /// Optional avatar `binary`/`image` field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        avatar_field: Option<FieldRef>,
        /// Optional palette color `integer` field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        color_field: Option<FieldRef>,
        /// Optional numeric count badge `integer` field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        count_field: Option<FieldRef>,
        /// Whether cards may be dragged onto another node to reassign `parent_field`.
        draggable: bool,
        /// Optional plural noun for child count pills (e.g. `"people"`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        label: Option<String>,
    },
    /// Stored-coordinate map root.
    Map {
        latitude: FieldRef,
        longitude: FieldRef,
    },
    /// Activity matrix root.
    Activity {
        /// Child arena indexes (record summary `field` nodes).
        children: Vec<u32>,
        /// Optional avatar `binary`/`image` field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        avatar_field: Option<FieldRef>,
    },
    /// One `<field>` of a graph: a grouping (first: the axis, second: the series) or a measure.
    GraphField {
        /// Accessible field handle.
        field: FieldRef,
        /// Display label.
        label: String,
        /// What the field does in the chart.
        role: GraphRole,
        /// Date grouping interval: day, week, month, quarter or year (date and datetime groupings only).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        interval: Option<String>,
    },
    /// Search field filter node.
    SearchField {
        /// Accessible field handle.
        field: FieldRef,
        /// Display label.
        label: String,
        /// Optional custom filter domain.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        filter_domain: Option<WireDocument>,
        /// Optional comparison operator.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        operator: Option<String>,
    },
    /// Predefined search filter or group-by filter.
    SearchFilter {
        /// Filter identifier.
        name: String,
        /// Display label.
        label: String,
        /// Optional domain expression.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        domain: Option<WireDocument>,
        /// Optional context expression.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        context: Option<WireDocument>,
    },
    /// OR-to-AND boundary between surrounding filters.
    SearchSeparator {},
    /// Labelled search filter group.
    SearchGroup {
        /// Display caption.
        label: String,
        /// Standard expand flag; false collapses filters under this group.
        #[serde(default)]
        expand: bool,
        /// Child arena indexes.
        children: Vec<u32>,
    },
}
choice!(NotificationLevel {
    Info,
    Success,
    Warning,
    Danger
});
/// Normalized result of an object-method call. The client decodes it strictly and never
/// interprets the raw dictionary the method returned.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-gen", erp_macros::wire_ts)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum ActionResult {
    /// The method returned null/true/false: nothing to dispatch.
    None {},
    /// Open a caller-scoped related list in the main action stack.
    CurrentWindow {
        /// Action title, displayed as text.
        name: String,
        /// Authorized registered target model.
        model: String,
        /// List, optionally followed by form; service views are not admitted. With `res_id`,
        /// exactly one form.
        views: Vec<ActionView>,
        /// Literal base domain. Later ORM requests still apply caller access rules. With
        /// `res_id`, exactly `[["id", "=", res_id]]`.
        #[cfg_attr(feature = "schema-gen", ts(type = "unknown[]"))]
        domain: Vec<serde_json::Value>,
        /// The one record to open as a form, readable by the caller when admitted; absent for a
        /// list.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schema-gen", ts(optional))]
        res_id: Option<WireId>,
        /// Bounded literal defaults; authority-bearing context is refused.
        #[cfg_attr(feature = "schema-gen", ts(type = "Record<string, unknown>"))]
        context: BTreeMap<String, serde_json::Value>,
    },
    /// Open a form in a modal dialog (`target = new`).
    Window {
        /// Dialog title.
        name: String,
        /// Authorized target model.
        model: String,
        /// Existing record to open, or `null` for a new record.
        #[serde(deserialize_with = "required_nullable")]
        res_id: Option<WireId>,
        /// One form view request; the producer emits exactly one and clients must refuse any other count.
        views: Vec<ActionView>,
        /// Literal `default_<field>` values only; keys and values are bounded by the producer.
        #[cfg_attr(feature = "schema-gen", ts(type = "Record<string, unknown>"))]
        context: BTreeMap<String, serde_json::Value>,
    },
    /// Close the current dialog.
    Close {},
    /// Show a text notification, optionally closing the dialog afterwards.
    Notification {
        /// Optional short heading.
        #[serde(deserialize_with = "required_nullable")]
        title: Option<String>,
        /// Notification text, never markup.
        message: String,
        /// Closed severity vocabulary.
        level: NotificationLevel,
        /// Whether the dialog closes after the notification.
        then_close: bool,
    },
    /// Mount a registered client action screen (`ir.actions.client`): `tag` names the screen the
    /// browser's registry binds; `params` are the tag's validated parameters.
    Client {
        /// The action's id when it was opened by id (menu, URL); null for a method result.
        #[serde(deserialize_with = "required_nullable")]
        id: Option<WireId>,
        /// Action title, displayed as text.
        name: String,
        /// Tag declared by an installed addon.
        tag: String,
        /// Normalized parameters, at most 16 KiB encoded.
        #[cfg_attr(feature = "schema-gen", ts(type = "Record<string, unknown>"))]
        params: BTreeMap<String, serde_json::Value>,
    },
    /// Print a report for records (an `ir.actions.report` result): the client sends it through
    /// its print request, which checks the report and the records again.
    Report {
        /// The report's name, shown while it prints.
        name: String,
        /// The report's technical name.
        report: String,
        /// The registered model of the printed records.
        model: String,
        /// The printed records, in order.
        ids: Vec<WireId>,
    },
}
object!(/// Versioned envelope of a normalized method result.
    ActionResultEnvelope { wire_major: WireMajor, result: ActionResult });
impl ActionResultEnvelope {
    /// Wrap a validated result at the supported wire major.
    pub fn new(result: ActionResult) -> Self {
        Self {
            wire_major: WireMajor::V1,
            result,
        }
    }
}
object!(/// Typed, public view-selection notice.
    Notice { code: String, message: String });
/// Exact declared settings scope; company membership still requires current server authorization.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-gen", erp_macros::wire_ts)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum SettingsScope {
    /// Database-local scope.
    Global {},
    /// Company-local scope.
    Company {
        /// Positive checked company identity.
        company_id: WireId,
    },
}
choice!(SettingKind {
    Boolean,
    String,
    Integer,
    Float,
    Selection
});
/// Scalar setting payload; declaration validation must enforce its kind and numeric bounds.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-gen", erp_macros::wire_ts)]
#[serde(untagged)]
#[non_exhaustive]
pub enum SettingValue {
    /// Boolean.
    Boolean(bool),
    /// Text or a declared selection key.
    String(String),
    /// Integer; must pass declaration's safe-client bound.
    Integer(#[cfg_attr(feature = "schema-gen", ts(type = "number"))] i64),
    /// Finite float.
    Float(f64),
}
object!(/// Authorized non-secret settings declarations, since, the page's information and link
    /// rows (`items`, absent in older payloads and then empty).
    SettingsDescription { wire_major: WireMajor, registry_version: String,
        scope: SettingsScope, declarations: Vec<SettingDescription>,
        #[serde(default)] items: Vec<SettingsItem> });
/// Target of a settings link row: a window action, optionally opening one record, or a menu.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-gen", erp_macros::wire_ts)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum SettingsLinkTarget {
    /// Open a window action.
    Action {
        /// Id of the `ir.actions.act_window` row.
        id: WireId,
        /// The record the action opens, or `null` to open it as declared.
        #[serde(deserialize_with = "required_nullable")]
        res_id: Option<WireId>,
    },
    /// Open a menu.
    Menu {
        /// Id of the `ir.ui.menu` row.
        id: WireId,
    },
}
choice!(ModuleToggleState {
    Installed,
    NotInstalled,
    NotAvailable
});
choice!(GroupToggleState {
    Enabled,
    Disabled,
    NotAvailable
});
/// One read-only information row, link row, module toggle or group toggle of a settings page. Items are never
/// settings: their keys are refused by `erp_read_values` and `erp_write_values`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-gen", erp_macros::wire_ts)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum SettingsItem {
    /// A value computed with the caller's rights.
    Info {
        /// Addon-qualified identity.
        key: String,
        /// Display label (the resolver may have chosen a singular or plural form).
        label: String,
        /// Settings app page.
        app: String,
        /// Block title within the page.
        block: String,
        /// Display sort sequence.
        sequence: u32,
        /// Scalar value, displayed as text.
        value: SettingValue,
        /// Secondary text under the value, lines separated by line feeds, or `null`.
        #[serde(deserialize_with = "required_nullable")]
        detail: Option<String>,
    },
    /// An arrow link to an action or a menu.
    Link {
        /// Addon-qualified identity.
        key: String,
        /// Display label.
        label: String,
        /// Settings app page.
        app: String,
        /// Block title within the page.
        block: String,
        /// Display sort sequence.
        sequence: u32,
        /// What the link opens.
        target: SettingsLinkTarget,
    },
    /// A toggle row that installs or uninstalls one addon.
    Module {
        /// Addon-qualified identity.
        key: String,
        /// Display label.
        label: String,
        /// Settings app page.
        app: String,
        /// Block title within the page.
        block: String,
        /// Display sort sequence.
        sequence: u32,
        /// Technical name of the addon controlled by this row.
        addon: String,
        /// Resolved availability and installation state of the addon.
        state: ModuleToggleState,
    },
    /// A toggle row that turns one feature group on or off for every internal user, changed through
    /// `ir.config.setting/erp_toggle_group`.
    Group {
        /// Addon-qualified identity.
        key: String,
        /// Display label.
        label: String,
        /// Settings app page.
        app: String,
        /// Block title within the page.
        block: String,
        /// Display sort sequence.
        sequence: u32,
        /// `<module>.<name>` identity of the feature group controlled by this row.
        group: String,
        /// Whether the internal-user group implies the feature group (directly or through other groups), or
        /// `not_available` when the database no longer lets this row work.
        state: GroupToggleState,
        /// Why the row is not available, or `null` when it is.
        #[serde(deserialize_with = "required_nullable")]
        reason: Option<String>,
        /// Whether the feature group is on for every internal user although the row is not available:
        /// the row can still be switched off, never on.
        on_for_all: bool,
    },
}
object!(/// One visible native declaration; policy internals stay on the server.
    SettingDescription { key: String, label: String, kind: SettingKind,
        default: SettingValue, #[serde(deserialize_with="required_nullable")] choices: Option<Vec<(String,String)>>, writable: bool,
        #[serde(default)] app: String,
        #[serde(default = "default_setting_block")] block: String,
        #[serde(default = "default_setting_sequence")] sequence: u32,
        #[serde(default, skip_serializing_if="Option::is_none")] help: Option<String> });

fn default_setting_block() -> String {
    "General".into()
}

const fn default_setting_sequence() -> u32 {
    100
}
choice!(ValueSource { Default, Stored });
object!(/// Scoped settings values and optimistic concurrency revision.
    SettingsValues { wire_major: WireMajor, scope: SettingsScope, revision: String,
        values: Vec<SettingEntry> });
object!(/// Validated setting value with explicit storage origin.
    SettingEntry { key: String, value: SettingValue, source: ValueSource });
choice!(AddonState {
    Installed,
    Uninstalled
});
object!(/// Authorized linked native addon catalogue.
    AddonCatalog { wire_major: WireMajor, registry_version: String, addons: Vec<AddonEntry> });
object!(/// Manifest-validated native addon and persisted state. `category` is the manifest category
    /// path (`/`-separated, root `Industries` for industry addons); `application` marks an app.
    /// `website` and `license` are the manifest values (empty when the manifest names none) and
    /// `depends` the direct dependencies by technical name, sorted: the Apps card's module info.
    /// `icon` is the manifest's `lucide:<name>`, omitted when the manifest declares none.
    AddonEntry { name: String, label: String, version: String, state: AddonState, installable: bool,
        summary: String, category: String, author: String, application: bool, website: String,
        license: String, depends: Vec<String>,
        #[serde(default, deserialize_with="present_optional", skip_serializing_if="Option::is_none")]
        #[cfg_attr(feature="schema-gen", ts(optional))] icon: Option<String> });
object!(/// Durable installation result; no success is sent before lifecycle commit/publication.
    InstallResult { installed: Vec<String>, registry_version: String, asset_version: String });

object!(/// Durable upgrade outcome, including only dependencies actually newly installed.
    UpgradeResult { upgraded: Vec<String>, installed: Vec<String>, registry_version: String, asset_version: String });
object!(/// Durable uninstall outcome, including only addons actually removed.
    UninstallResult { uninstalled: Vec<String>, registry_version: String, asset_version: String });
/// Detail page admission size. Omitted request limits default to 50; supplied limits are 1–100.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-gen", erp_macros::wire_ts)]
#[serde(try_from = "u64", into = "u64")]
pub struct AddonDetailLimit(#[cfg_attr(feature = "schema-gen", ts(type = "number"))] u64);
impl TryFrom<u64> for AddonDetailLimit {
    type Error = &'static str;
    fn try_from(value: u64) -> Result<Self, Self::Error> {
        if (1..=100).contains(&value) {
            Ok(Self(value))
        } else {
            Err("addon detail limit must be between 1 and 100")
        }
    }
}
impl From<AddonDetailLimit> for u64 {
    fn from(value: AddonDetailLimit) -> Self {
        value.0
    }
}
object!(/// Strict detail read arguments. Optional pagination fields may be absent, never null.
    AddonDetailRequest { name: String,
        #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="optional_present")] #[cfg_attr(feature="schema-gen",ts(optional))] limit: Option<AddonDetailLimit>,
        #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="optional_present")] #[cfg_attr(feature="schema-gen",ts(optional))] dependencies_cursor: Option<String>,
        #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="optional_present")] #[cfg_attr(feature="schema-gen",ts(optional))] dependents_cursor: Option<String>,
        #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="optional_present")] #[cfg_attr(feature="schema-gen",ts(optional))] menus_cursor: Option<String>,
        #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="optional_present")] #[cfg_attr(feature="schema-gen",ts(optional))] views_cursor: Option<String> });

choice!(AddonDetailState {
    Installed,
    Uninstalled,
    ToInstall,
    ToUpgrade,
    ToRemove,
    Uninstallable
});
object!(/// Bounded dependency page; a null cursor explicitly marks the final page.
    AddonNamePage { items: Vec<String>, #[serde(deserialize_with="required_nullable")] next_cursor: Option<String> });
object!(/// Menu attributed to this addon by durable metadata provenance.
    AddonMenu { id: WireId, name: String, label: String });
object!(/// View attributed to this addon by durable metadata provenance.
    AddonView { id: WireId, name: String, model: String, kind: String });
object!(/// Bounded menu metadata page.
    AddonMenuPage { items: Vec<AddonMenu>, #[serde(deserialize_with="required_nullable")] next_cursor: Option<String> });
object!(/// Bounded view metadata page.
    AddonViewPage { items: Vec<AddonView>, #[serde(deserialize_with="required_nullable")] next_cursor: Option<String> });
object!(/// Authorized addon detail. Text is plain text; visibility never grants executability.
    /// Pages use deterministic cursors bound to addon, collection and authoritative revision.
    AddonDetail { wire_major: WireMajor, registry_version: String, name: String,
        label: String, summary: String, description: String, author: String,
        #[serde(deserialize_with="required_nullable")] available_version: Option<String>,
        #[serde(deserialize_with="required_nullable")] installed_version: Option<String>,
        state: AddonDetailState, dependencies: AddonNamePage, dependents: AddonNamePage,
        menus: AddonMenuPage, views: AddonViewPage,
        #[serde(deserialize_with="required_nullable")] upgrade_refusal: Option<String>,
        #[serde(deserialize_with="required_nullable")] uninstall_refusal: Option<String> });

/// Lossless canonical unsigned 64-bit count, transported as decimal text rather than JSON number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-gen", erp_macros::wire_ts)]
#[serde(try_from = "String", into = "String")]
pub struct WireDecimalCount(#[cfg_attr(feature = "schema-gen", ts(type = "string"))] String);
impl TryFrom<String> for WireDecimalCount {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty()
            || value.len() > 20
            || (value.len() > 1 && value.starts_with('0'))
            || !value.bytes().all(|byte| byte.is_ascii_digit())
            || value.parse::<u64>().is_err()
        {
            return Err("count must be a canonical unsigned 64-bit decimal string");
        }
        Ok(Self(value))
    }
}
impl From<WireDecimalCount> for String {
    fn from(value: WireDecimalCount) -> Self {
        value.0
    }
}
impl From<u64> for WireDecimalCount {
    fn from(value: u64) -> Self {
        Self(value.to_string())
    }
}

fn optional_present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

// Optional means omitted, not a present null or a substituted default.
fn present_optional<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

impl LoginPage {
    /// Construct the server-authoritative login document; redirect is normalized by transport.
    pub fn new(csrf: String, database: String, redirect: String, error: Option<String>) -> Self {
        Self {
            csrf,
            database,
            redirect,
            error,
            external_label: None,
            can_link_external: None,
        }
    }

    /// Publish a configured external sign-in option. Linking is available only with a native session.
    #[must_use]
    pub fn with_external_provider(mut self, label: String, can_link: bool) -> Self {
        self.external_label = Some(label);
        self.can_link_external = Some(can_link);
        self
    }
}

impl MenuTree {
    /// Construct an ordered, caller-authorized menu forest.
    pub fn new(registry_version: String, roots: Vec<WireId>, nodes: Vec<MenuNode>) -> Self {
        Self {
            wire_major: WireMajor::V1,
            registry_version,
            roots,
            nodes,
        }
    }
}
impl MenuNode {
    /// Construct one menu node with its already-filtered visible children.
    pub fn new(
        id: WireId,
        parent_id: Option<WireId>,
        children: Vec<WireId>,
        label: String,
        icon: Option<String>,
        action_id: Option<WireId>,
    ) -> Self {
        Self {
            id,
            parent_id,
            children,
            label,
            icon,
            action_kind: action_id.map(|_| MenuActionKind::Window),
            action_id,
            module: None,
        }
    }

    /// Mark the menu's action as an `ir.actions.client`.
    pub fn with_client_action(mut self) -> Self {
        if self.action_id.is_some() {
            self.action_kind = Some(MenuActionKind::Client);
        }
        self
    }

    /// Attach this menu's resolved provenance namespace, independently of its parent/action.
    /// `None` denotes missing provenance or an empty namespace on the oldest external ID.
    pub fn with_module(mut self, module: Option<String>) -> Self {
        self.module = module;
        self
    }
}
impl ActionView {
    /// Describe one requested window-action view.
    pub fn new(id: Option<WireId>, view_type: ViewType) -> Self {
        Self { id, view_type }
    }
}
impl WindowAction {
    /// Construct a fully authorized window action.
    pub fn new(
        id: WireId,
        name: String,
        model: String,
        target: ActionTarget,
        views: Vec<ActionView>,
        domain: Option<WireDocument>,
        context: Option<WireDocument>,
    ) -> Self {
        Self {
            wire_major: WireMajor::V1,
            id,
            name,
            model,
            target,
            views,
            domain,
            context,
        }
    }
}
impl WindowActionViews {
    /// Bundle descriptions only after both action and view authorization succeeds.
    pub fn new(action: WindowAction, views: ViewDescription) -> Self {
        Self { action, views }
    }
}
impl FieldDescription {
    /// Describe a field only after caller field-read authorization succeeds. `relation_field` is
    /// the comodel's inverse many2one of a one2many, and `None` for every other kind.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        field_type: FieldKind,
        label: String,
        required: bool,
        readonly: bool,
        selection: Option<Vec<(String, String)>>,
        relation: Option<String>,
        relation_field: Option<String>,
    ) -> Self {
        Self {
            field_type,
            label,
            required,
            readonly,
            selection,
            relation,
            relation_field,
        }
    }
}
impl ViewIdentity {
    /// A persisted `ir.ui.view` row.
    pub fn stored(id: WireId) -> Self {
        Self::Stored { id }
    }
    /// A generated view with no persisted identity.
    pub fn generated() -> Self {
        Self::Generated {}
    }
}
impl View {
    /// Construct one serialized view arena.
    pub fn new(
        identity: ViewIdentity,
        view_type: ViewType,
        renderer: String,
        root: u32,
        nodes: Vec<ViewNode>,
    ) -> Self {
        Self {
            identity,
            view_type,
            schema_major: WireMajor::V1,
            renderer,
            root,
            nodes,
        }
    }
}
impl FieldRef {
    /// Construct an accessible model field handle.
    pub fn new(model: String, name: String) -> Self {
        Self { model, name }
    }
}
impl Modifiers {
    /// No supported dynamic modifiers.
    pub fn empty() -> Self {
        Self {
            readonly: None,
            required: None,
            invisible: None,
        }
    }
}
impl EmptyOptions {
    /// Empty options for the initial field-widget contract.
    pub fn new() -> Self {
        Self {}
    }
}
impl Default for EmptyOptions {
    fn default() -> Self {
        Self::new()
    }
}
impl ViewDescription {
    /// Construct the accessible descriptions for one model.
    pub fn new(
        registry_version: String,
        description_version: String,
        model: String,
        fields: BTreeMap<String, BTreeMap<String, FieldDescription>>,
        views: Vec<View>,
        notices: Vec<Notice>,
    ) -> Self {
        Self {
            wire_major: WireMajor::V1,
            registry_version,
            description_version,
            model,
            fields,
            views,
            notices,
        }
    }
}
impl Notice {
    /// A typed view-selection notice.
    pub fn new(code: String, message: String) -> Self {
        Self { code, message }
    }
}

impl SettingsDescription {
    /// Create an authorized settings description for one exact scope.
    pub fn new(
        registry_version: String,
        scope: SettingsScope,
        declarations: Vec<SettingDescription>,
    ) -> Self {
        Self {
            wire_major: WireMajor::V1,
            registry_version,
            scope,
            declarations,
            items: Vec::new(),
        }
    }
    /// Attach the permitted information and link rows, already sorted by key.
    #[must_use]
    pub fn with_items(mut self, items: Vec<SettingsItem>) -> Self {
        self.items = items;
        self
    }
}
impl SettingDescription {
    /// Attach bounded native declaration help, displayed as text by the client.
    pub fn with_help(mut self, help: Option<String>) -> Self {
        self.help = help;
        self
    }
    /// Describe one permitted setting without exposing its group policy.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        key: String,
        label: String,
        kind: SettingKind,
        default: SettingValue,
        choices: Option<Vec<(String, String)>>,
        writable: bool,
        app: String,
        block: String,
        sequence: u32,
    ) -> Self {
        Self {
            key,
            label,
            kind,
            default,
            choices,
            writable,
            app,
            block,
            sequence,
            help: None,
        }
    }
}
impl SettingsValues {
    /// Create ordered values at the observed exact-scope revision.
    pub fn new(scope: SettingsScope, revision: String, values: Vec<SettingEntry>) -> Self {
        Self {
            wire_major: WireMajor::V1,
            scope,
            revision,
            values,
        }
    }
}
impl SettingEntry {
    /// Create one validated value with its explicit origin.
    pub fn new(key: String, value: SettingValue, source: ValueSource) -> Self {
        Self { key, value, source }
    }
}

/// Closed scalar kinds accepted by builtin field presentation options.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum FieldOptionKind {
    /// Bounded text.
    Text,
    /// Explicit boolean.
    Bool,
    /// Bounded integer.
    Integer,
    /// Authorized field handle.
    Field,
    /// Bounded text choices.
    Strings,
    /// Precision and scale.
    Digits,
    /// Width and height in pixels.
    Size,
    /// Existing expression document.
    Expression,
}
/// Resolve an option only when the named builtin widget owns its semantics.
/// Unknown widget-specific options are refused, including options on trusted addon widgets.
pub fn field_option_kind(widget: &str, key: &str) -> Option<FieldOptionKind> {
    let text = matches!(
        widget,
        "char" | "text" | "password" | "email" | "phone" | "url"
    );
    let numeric = matches!(widget, "integer" | "float" | "monetary" | "progressbar");
    let percentage = widget == "percentage";
    let date = matches!(widget, "date" | "datetime" | "daterange");
    let image = matches!(widget, "image" | "image_url");
    let relation = matches!(
        widget,
        "many2one"
            | "many2one_avatar"
            | "one2many"
            | "many2many"
            | "many2many_tags"
            | "many2many_tags_avatar"
            | "many2many_tags_color_dot"
    );
    let selection_editor = matches!(
        widget,
        "selection" | "badges_selection" | "filterable_selection"
    );
    let admitted = match key {
        "help" => true,
        "placeholder" => text || numeric || date || relation || selection_editor,
        "autocomplete" | "password" | "max_length" => text,
        "horizontal" => widget == "radio",
        "rows" => widget == "text",
        "type" => numeric,
        "digits" | "hide_trailing_zeros" => numeric || percentage,
        "enable_formatting" => matches!(widget, "integer" | "float"),
        "no_symbol" => widget == "monetary" || percentage,
        "currency_field" | "field_digits" => widget == "monetary",
        "placeholder_field" => text || date || relation || selection_editor,
        "icon" | "icon_class" => widget == "boolean_icon",
        "nolabel" => relation,
        "no_open" | "no_create" | "no_quick_create" | "no_create_edit" | "search_threshold"
        | "create" | "domain" | "context" => relation,
        "can_open" | "color_field" | "tag_limit" => matches!(
            widget,
            "many2many" | "many2many_tags" | "many2many_tags_avatar" | "many2many_tags_color_dot"
        ),
        "min_date" | "max_date" | "min_precision" | "max_precision" | "warn" | "numeric"
        | "show_time" | "show_seconds" | "rounding" | "start_date_field" | "end_date_field"
        | "always_range" => date,
        "badge_limit" => widget == "badges_selection",
        "whitelisted_values" | "blacklisted_values" | "whitelist_fname" => {
            widget == "filterable_selection"
        }
        "text" => widget == "url",
        // The colour of each value of a badge, declared by the view (`tone_<name>`: the values shown in that tone).
        "tone_success" | "tone_info" | "tone_warning" | "tone_danger" | "tone_muted" => {
            matches!(widget, "badge" | "label_selection")
        }
        "alt" | "img_class" | "zoom" | "zoom_delay" | "size" | "reload" => image,
        "accepted_file_extensions" => matches!(widget, "image" | "binary"),
        "convert_to_webp" | "preview_image" => widget == "image",
        "filename" => widget == "binary",
        _ => false,
    };
    if !admitted {
        return None;
    }
    Some(match key {
        "help" => FieldOptionKind::Text,
        "placeholder" => FieldOptionKind::Text,
        "autocomplete" => FieldOptionKind::Text,
        "icon" => FieldOptionKind::Text,
        "icon_class" => FieldOptionKind::Text,
        "min_date" => FieldOptionKind::Text,
        "max_date" => FieldOptionKind::Text,
        "min_precision" => FieldOptionKind::Text,
        "max_precision" => FieldOptionKind::Text,
        "warn" => FieldOptionKind::Text,
        "type" => FieldOptionKind::Text,
        "text" => FieldOptionKind::Text,
        "password" => FieldOptionKind::Bool,
        "nolabel" => FieldOptionKind::Bool,
        "no_open" => FieldOptionKind::Bool,
        "no_create" => FieldOptionKind::Bool,
        "no_quick_create" => FieldOptionKind::Bool,
        "no_create_edit" => FieldOptionKind::Bool,
        "no_symbol" => FieldOptionKind::Bool,
        "enable_formatting" => FieldOptionKind::Bool,
        "hide_trailing_zeros" => FieldOptionKind::Bool,
        "field_digits" => FieldOptionKind::Bool,
        "show_time" => FieldOptionKind::Bool,
        "show_seconds" => FieldOptionKind::Bool,
        "always_range" => FieldOptionKind::Bool,
        "numeric" => FieldOptionKind::Bool,
        "create" => FieldOptionKind::Bool,
        "can_open" => FieldOptionKind::Bool,
        "horizontal" => FieldOptionKind::Bool,
        "rows" => FieldOptionKind::Integer,
        "max_length" => FieldOptionKind::Integer,
        "badge_limit" => FieldOptionKind::Integer,
        "tag_limit" => FieldOptionKind::Integer,
        "search_threshold" => FieldOptionKind::Integer,
        "rounding" => FieldOptionKind::Integer,
        "placeholder_field" => FieldOptionKind::Field,
        "currency_field" => FieldOptionKind::Field,
        "start_date_field" => FieldOptionKind::Field,
        "end_date_field" => FieldOptionKind::Field,
        "whitelist_fname" => FieldOptionKind::Field,
        "color_field" => FieldOptionKind::Field,
        "whitelisted_values" => FieldOptionKind::Strings,
        "blacklisted_values" => FieldOptionKind::Strings,
        "tone_success" | "tone_info" | "tone_warning" | "tone_danger" | "tone_muted" => {
            FieldOptionKind::Strings
        }
        "digits" => FieldOptionKind::Digits,
        "size" => FieldOptionKind::Size,
        "alt" | "img_class" | "accepted_file_extensions" => FieldOptionKind::Text,
        "zoom" | "reload" | "convert_to_webp" => FieldOptionKind::Bool,
        "zoom_delay" => FieldOptionKind::Integer,
        "preview_image" | "filename" => FieldOptionKind::Field,
        "domain" | "context" => FieldOptionKind::Expression,
        _ => return None,
    })
}
