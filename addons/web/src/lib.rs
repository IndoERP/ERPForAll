//! The `web` addon (layer L11): the web client's server side and, under
//! `static/`, its client code.
//!
//! The crate exposes native menus, window actions and list/form descriptions through ordinary
//! authorized RPCs. Its client package lives under `static/`; assets and other view families are
//! delivered by their own slices. It depends on `base` and is installed with every database:
//!
//! ```
//! let registry = erp_sdk::RegistryBuilder::new([&erp_addon_web::ADDON]).build()?;
//! assert_eq!(erp_addon_web::ADDON.name, "web");
//! assert_eq!(erp_addon_web::ADDON.depends, ["base"]);
//! assert!(registry.model("res.users").is_some());
//! # Ok::<(), erp_sdk::Error>(())
//! ```

erp_sdk::addon!(name = "web", depends = ["base"]);

pub mod assets;

mod action_result;
mod h18_authorize;
mod h18_lower;
mod h18_storage;
#[cfg(test)]
mod h18_storage_unit;
mod h18_transport;
#[cfg(test)]
mod h18_transport_unit;
mod metadata;
mod view_types;

/// `web.menu_icon`: the client's icon picker for a menu's `web_icon` text (`lucide:<name>`).
#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::views::FIELD_WIDGETS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static MENU_ICON_WIDGET: erp_sdk::views::WidgetDecl =
    erp_sdk::views::WidgetDecl::new("web.menu_icon", &["char"]);

/// `web.preference_choice`: the My Preferences dialog's language and time zone picker, a text
/// field shown as a selection over the `lang`/`tz` choices that `res.partner.fields_get` lists.
#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::views::FIELD_WIDGETS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static PREFERENCE_CHOICE_WIDGET: erp_sdk::views::WidgetDecl =
    erp_sdk::views::WidgetDecl::new("web.preference_choice", &["char"]);

/// `statusbar`: metadata-driven selection state in a record form header.
#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::views::FIELD_WIDGETS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static STATUSBAR_WIDGET: erp_sdk::views::WidgetDecl =
    erp_sdk::views::WidgetDecl::new("statusbar", &["selection"]);

/// Native admission for the password field adapter.
#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::views::FIELD_WIDGETS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static PASSWORD_WIDGET: erp_sdk::views::WidgetDecl =
    erp_sdk::views::WidgetDecl::new("password", &["char", "text"]);

/// Native admission for the email field adapter.
#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::views::FIELD_WIDGETS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static EMAIL_WIDGET: erp_sdk::views::WidgetDecl =
    erp_sdk::views::WidgetDecl::new("email", &["char"]);

/// Native admission for the phone field adapter.
#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::views::FIELD_WIDGETS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static PHONE_WIDGET: erp_sdk::views::WidgetDecl =
    erp_sdk::views::WidgetDecl::new("phone", &["char"]);

/// Native admission for the url field adapter.
#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::views::FIELD_WIDGETS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static URL_WIDGET: erp_sdk::views::WidgetDecl = erp_sdk::views::WidgetDecl::new("url", &["char"]);

/// Native admission for the boolean_checkbox field adapter.
#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::views::FIELD_WIDGETS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static BOOL_CHECK_WIDGET: erp_sdk::views::WidgetDecl =
    erp_sdk::views::WidgetDecl::new("boolean_checkbox", &["boolean"]);

/// Native admission for the boolean_toggle field adapter.
#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::views::FIELD_WIDGETS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static BOOL_TOGGLE_WIDGET: erp_sdk::views::WidgetDecl =
    erp_sdk::views::WidgetDecl::new("boolean_toggle", &["boolean"]);

/// Native admission for the boolean_favorite field adapter.
#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::views::FIELD_WIDGETS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static BOOL_FAV_WIDGET: erp_sdk::views::WidgetDecl =
    erp_sdk::views::WidgetDecl::new("boolean_favorite", &["boolean"]);

/// Native admission for the boolean_icon field adapter.
#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::views::FIELD_WIDGETS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static BOOL_ICON_WIDGET: erp_sdk::views::WidgetDecl =
    erp_sdk::views::WidgetDecl::new("boolean_icon", &["boolean"]);

/// Native admission for the radio field adapter.
#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::views::FIELD_WIDGETS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static RADIO_WIDGET: erp_sdk::views::WidgetDecl =
    erp_sdk::views::WidgetDecl::new("radio", &["selection", "many2one"]);

/// Native admission for the badge field adapter.
#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::views::FIELD_WIDGETS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static BADGE_WIDGET: erp_sdk::views::WidgetDecl =
    erp_sdk::views::WidgetDecl::new("badge", &["selection", "many2one", "char"]);

/// Native admission for the label_selection field adapter.
#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::views::FIELD_WIDGETS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static LABEL_SELECTION_WIDGET: erp_sdk::views::WidgetDecl =
    erp_sdk::views::WidgetDecl::new("label_selection", &["selection"]);

/// Native admission for the badges_selection field adapter.
#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::views::FIELD_WIDGETS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static BADGES_SELECTION_WIDGET: erp_sdk::views::WidgetDecl =
    erp_sdk::views::WidgetDecl::new("badges_selection", &["selection"]);

/// Native admission for the filterable_selection field adapter.
#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::views::FIELD_WIDGETS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static FILTER_SELECTION_WIDGET: erp_sdk::views::WidgetDecl =
    erp_sdk::views::WidgetDecl::new("filterable_selection", &["selection"]);

/// Native admission for the percentage field adapter (stored fraction shown and edited as a percent).
#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::views::FIELD_WIDGETS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static PERCENTAGE_WIDGET: erp_sdk::views::WidgetDecl =
    erp_sdk::views::WidgetDecl::new("percentage", &["integer", "float"]);

/// Native admission for the priority field adapter.
#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::views::FIELD_WIDGETS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static PRIORITY_WIDGET: erp_sdk::views::WidgetDecl =
    erp_sdk::views::WidgetDecl::new("priority", &["selection"]);

/// Native admission for the color field adapter.
#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::views::FIELD_WIDGETS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static COLOR_WIDGET: erp_sdk::views::WidgetDecl =
    erp_sdk::views::WidgetDecl::new("color", &["char"]);

/// Native admission for the color_picker field adapter.
#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::views::FIELD_WIDGETS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static COLOR_PICKER_WIDGET: erp_sdk::views::WidgetDecl =
    erp_sdk::views::WidgetDecl::new("color_picker", &["integer"]);

/// Native admission for the CopyClipboardChar field adapter.
#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::views::FIELD_WIDGETS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static COPY_CLIPBOARD_CHAR_WIDGET: erp_sdk::views::WidgetDecl =
    erp_sdk::views::WidgetDecl::new("CopyClipboardChar", &["char"]);

/// Native admission for the CopyClipboardText field adapter.
#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::views::FIELD_WIDGETS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static COPY_CLIPBOARD_TEXT_WIDGET: erp_sdk::views::WidgetDecl =
    erp_sdk::views::WidgetDecl::new("CopyClipboardText", &["text"]);

/// Native admission for the CopyClipboardURL field adapter.
#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::views::FIELD_WIDGETS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static COPY_CLIPBOARD_URL_WIDGET: erp_sdk::views::WidgetDecl =
    erp_sdk::views::WidgetDecl::new("CopyClipboardURL", &["char"]);

/// Native admission for the daterange field adapter.
#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::views::FIELD_WIDGETS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static DATE_RANGE_WIDGET: erp_sdk::views::WidgetDecl =
    erp_sdk::views::WidgetDecl::new("daterange", &["date", "datetime"]);

/// Native admission for the many2many_tags field adapter.
#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::views::FIELD_WIDGETS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static TAGS_WIDGET: erp_sdk::views::WidgetDecl =
    erp_sdk::views::WidgetDecl::new("many2many_tags", &["many2many"]);

/// Native admission for the many2many_tags_avatar field adapter.
#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::views::FIELD_WIDGETS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static TAGS_AVATAR_WIDGET: erp_sdk::views::WidgetDecl =
    erp_sdk::views::WidgetDecl::new("many2many_tags_avatar", &["many2many"]);

/// Native admission for the many2many_tags_color_dot field adapter.
#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::views::FIELD_WIDGETS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static TAGS_DOT_WIDGET: erp_sdk::views::WidgetDecl =
    erp_sdk::views::WidgetDecl::new("many2many_tags_color_dot", &["many2many"]);

/// `code`: source-text presentation with closed, non-executing modes.
#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::views::FIELD_WIDGETS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static CODE_WIDGET: erp_sdk::views::WidgetDecl = erp_sdk::views::WidgetDecl::new("code", &["text"]);

/// `list` view type (H18).
#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::views::VIEW_TYPES)]
#[linkme(crate = erp_sdk::__private::linkme)]
pub static LIST_VIEW_TYPE: erp_sdk::views::ViewTypeDeclFn = view_types::list_view_type;

/// `form` view type (H18).
#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::views::VIEW_TYPES)]
#[linkme(crate = erp_sdk::__private::linkme)]
pub static FORM_VIEW_TYPE: erp_sdk::views::ViewTypeDeclFn = view_types::form_view_type;

/// `search` view type (H18).
#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::views::VIEW_TYPES)]
#[linkme(crate = erp_sdk::__private::linkme)]
pub static SEARCH_VIEW_TYPE: erp_sdk::views::ViewTypeDeclFn = view_types::search_view_type;

/// The native H18 declarations owned by this exact web addon descriptor.
///
/// Composition roots pass these contributions to `ModuleSystemBuilder::view_types` along with
/// `ADDON`. A linked registration alone does not publish a serving contract; the module system
/// validates the installed owner and rebuilds the table for every registry generation.
///
/// ```
/// let contributions = erp_addon_web::view_type_contributions();
/// assert!(std::ptr::eq(contributions[0].owner(), &erp_addon_web::ADDON));
/// ```
pub fn view_type_contributions() -> [erp_sdk::views::ViewTypeContribution; 1] {
    static DECLARATIONS: [erp_sdk::views::ViewTypeDeclFn; 3] = [
        view_types::list_view_type,
        view_types::form_view_type,
        view_types::search_view_type,
    ];
    [erp_sdk::views::ViewTypeContribution::new(
        &ADDON,
        &DECLARATIONS,
    )]
}

/// Trusted integration-test entry points. Never registered as RPCs or included by default.
#[cfg(feature = "test-support")]
pub mod metadata_test_support {
    pub use crate::metadata::{
        test_get_views, test_load_action, test_load_action_probed, test_load_action_views,
        test_load_menus, test_load_menus_probed,
    };
}
