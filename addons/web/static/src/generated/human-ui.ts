// Generated from erp-views::wire by human_ui_schema. Do not edit.
import type { WireDocument } from '../../../../../web/src/generated/WireDocument';
export type WireId = number;
export type WirePageLimit = number;
export type WireMajor = 1;
export type LoginPage = {
/** Wire member `csrf`. */
csrf: string,
/** Wire member `database`. */
database: string,
/** Wire member `redirect`. */
redirect: string,
/** Wire member `error`. */
error: string | null, };
export type Bootstrap = {
/** Wire member `wire_major`. */
wire_major: WireMajor,
/** Wire member `registry_version`. */
registry_version: string,
/** Wire member `asset_version`. */
asset_version: string,
/** Wire member `identity`. */
identity: Identity,
/** Wire member `companies`. */
companies: Array<Company>,
/** Wire member `current_company_id`. */
current_company_id: WireId,
/** Wire member `allowed_company_ids`. */
allowed_company_ids: Array<WireId>,
/** Wire member `active_company_ids`. */
active_company_ids: Array<WireId>,
/** Wire member `locale`. */
locale: Locale,
/** Wire member `csrf`. */
csrf: Csrf,
/** Wire member `renderers`. */
renderers: Array<Renderer>, };
export type Identity = {
/** Wire member `uid`. */
uid: WireId,
/** Wire member `name`. */
name: string,
/** Wire member `internal`. */
internal: boolean, };
export type Company = {
/** Wire member `id`. */
id: WireId,
/** Wire member `name`. */
name: string,
/** Wire member `parent_id`. */
parent_id: WireId | null, };
export type Locale = {
/** Wire member `lang`. */
lang: string,
/** Wire member `tz`. */
tz: string,
/** Wire member `decimal_point`. */
decimal_point: string,
/** Wire member `thousands_sep`. */
thousands_sep: string,
/** Wire member `grouping`. */
grouping: Array<number>,
/** Wire member `date_format`. */
date_format: string,
/** Wire member `time_format`. */
time_format: string, };
export type Csrf = {
/** Wire member `logout`. */
logout: string, };
export type Renderer = {
/** Wire member `key`. */
key: string,
/** Wire member `schema_major`. */
schema_major: WireMajor, };
export type MenuTree = {
/** Wire member `wire_major`. */
wire_major: WireMajor,
/** Wire member `registry_version`. */
registry_version: string,
/** Wire member `roots`. */
roots: Array<WireId>,
/** Wire member `nodes`. */
nodes: Array<MenuNode>, };
export type MenuNode = {
/** Wire member `id`. */
id: WireId,
/** Wire member `parent_id`. */
parent_id: WireId | null,
/** Wire member `children`. */
children: Array<WireId>,
/** Wire member `label`. */
label: string,
/** Wire member `icon`. */
icon: string | null,
/** Wire member `action_id`. */
action_id: WireId | null,
/**
 *Wire member `module`.
 * Nonempty namespace of the oldest menu external ID, or null when absent/empty; never a permission grant.
 */
module: string | null, };
export type ActionTarget = "current" | "main" | "new";
export type ViewType = "list" | "form" | "search" | "kanban" | "calendar" | "pivot" | "graph" | "gantt" | "activity";
export type WindowAction = {
/** Wire member `wire_major`. */
wire_major: WireMajor,
/** Wire member `id`. */
id: WireId,
/** Wire member `name`. */
name: string,
/** Wire member `model`. */
model: string,
/** Wire member `target`. */
target: ActionTarget,
/** Wire member `views`. */
views: Array<ActionView>,
/** Wire member `domain`. */
domain: WireDocument | null,
/** Wire member `context`. */
context: WireDocument | null, };
export type WindowActionViews = {
/** Wire member `action`. */
action: WindowAction,
/** Wire member `views`. */
views: ViewDescription, };
export type ActionView = {
/** Wire member `id`. */
id: WireId | null,
/** Wire member `view_type`. */
type: ViewType, };
export type ViewDescription = {
/** Wire member `wire_major`. */
wire_major: WireMajor,
/** Wire member `registry_version`. */
registry_version: string,
/** Wire member `description_version`. */
description_version: string,
/** Wire member `model`. */
model: string,
/** Wire member `fields`. */
fields: { [key in string]?: { [key in string]?: FieldDescription } },
/** Wire member `views`. */
views: Array<View>,
/** Wire member `notices`. */
notices: Array<Notice>, };
export type FieldKind = "char" | "text" | "html" | "boolean" | "integer" | "float" | "monetary" | "date" | "datetime" | "selection" | "many2one" | "one2many" | "many2many" | "binary" | "json";
export type FieldDescription = {
/** Wire member `field_type`. */
type: FieldKind,
/** Wire member `label`. */
label: string,
/** Wire member `required`. */
required: boolean,
/** Wire member `readonly`. */
readonly: boolean,
/** Wire member `selection`. */
selection: Array<[string, string]> | null,
/** Wire member `relation`. */
relation: string | null,
/** Wire member `relation_field`. */
relation_field: string | null, };
export type ViewIdentity = { "kind": "stored",
/**
 * Positive identity.
 */
id: WireId, } | { "kind": "generated", };
export type View = {
/** Wire member `identity`. */
identity: ViewIdentity,
/** Wire member `view_type`. */
type: ViewType,
/** Wire member `schema_major`. */
schema_major: WireMajor,
/** Wire member `renderer`. */
renderer: string,
/** Wire member `root`. */
root: number,
/** Wire member `nodes`. */
nodes: Array<ViewNode>, };
export type FieldRef = {
/** Wire member `model`. */
model: string,
/** Wire member `name`. */
name: string, };
export type Modifiers = {
/** Wire member `readonly`. */
readonly: WireDocument | null,
/** Wire member `required`. */
required: WireDocument | null,
/** Wire member `invisible`. */
invisible: WireDocument | null, };
export type EmptyOptions = Record<string, never>;
export type StatusbarOptions = {
/** Wire member `visible`. */
visible: Array<string> | null, };
export type CodeMode = "xml" | "python" | "javascript" | "json" | "qweb" | "scss" | "bash" | "text";
export type CodeOptions = {
/** Wire member `mode`. */
mode: CodeMode, };
export type FieldPresentationOptions = {
/** Wire member `help`. */
help?: string | null,
/** Wire member `placeholder`. */
placeholder?: string | null,
/** Wire member `autocomplete`. */
autocomplete?: string | null,
/** Wire member `icon`. */
icon?: string | null,
/** Wire member `icon_class`. */
icon_class?: string | null,
/** Wire member `min_date`. */
min_date?: string | null,
/** Wire member `max_date`. */
max_date?: string | null,
/** Wire member `min_precision`. */
min_precision?: string | null,
/** Wire member `max_precision`. */
max_precision?: string | null,
/** Wire member `warn`. */
warn?: string | null,
/** Wire member `r#type`. */
type?: string | null,
/** Wire member `text`. */
text?: string | null,
/** Wire member `password`. */
password?: boolean | null,
/** Wire member `nolabel`. */
nolabel?: boolean | null,
/** Wire member `no_open`. */
no_open?: boolean | null,
/** Wire member `no_create`. */
no_create?: boolean | null,
/** Wire member `no_quick_create`. */
no_quick_create?: boolean | null,
/** Wire member `no_create_edit`. */
no_create_edit?: boolean | null,
/** Wire member `no_symbol`. */
no_symbol?: boolean | null,
/** Wire member `enable_formatting`. */
enable_formatting?: boolean | null,
/** Wire member `hide_trailing_zeros`. */
hide_trailing_zeros?: boolean | null,
/** Wire member `field_digits`. */
field_digits?: boolean | null,
/** Wire member `show_time`. */
show_time?: boolean | null,
/** Wire member `show_seconds`. */
show_seconds?: boolean | null,
/** Wire member `always_range`. */
always_range?: boolean | null,
/** Wire member `numeric`. */
numeric?: boolean | null,
/** Wire member `create`. */
create?: boolean | null,
/** Wire member `can_open`. */
can_open?: boolean | null,
/** Wire member `horizontal`. */
horizontal?: boolean | null,
/** Wire member `rows`. */
rows?: number | null,
/** Wire member `max_length`. */
max_length?: number | null,
/** Wire member `badge_limit`. */
badge_limit?: number | null,
/** Wire member `tag_limit`. */
tag_limit?: number | null,
/** Wire member `search_threshold`. */
search_threshold?: number | null,
/** Wire member `rounding`. */
rounding?: number | null,
/** Wire member `placeholder_field`. */
placeholder_field?: FieldRef | null,
/** Wire member `currency_field`. */
currency_field?: FieldRef | null,
/** Wire member `start_date_field`. */
start_date_field?: FieldRef | null,
/** Wire member `end_date_field`. */
end_date_field?: FieldRef | null,
/** Wire member `whitelist_fname`. */
whitelist_fname?: FieldRef | null,
/** Wire member `color_field`. */
color_field?: FieldRef | null,
/** Wire member `whitelisted_values`. */
whitelisted_values?: Array<string> | null,
/** Wire member `blacklisted_values`. */
blacklisted_values?: Array<string> | null,
/** Wire member `digits`. */
digits?: number[] | null,
/** Wire member `domain`. */
domain?: WireDocument | null,
/** Wire member `context`. */
context?: WireDocument | null, };
export type FieldOptions = CodeOptions | FieldPresentationOptions | StatusbarOptions | EmptyOptions;
export type RecordMode = "record" | "service";
export type PivotRole = "row" | "col" | "measure";
export type ButtonType = "object";
export type ButtonVariant = "primary" | "secondary" | "link" | "success" | "info" | "warning" | "danger" | "unstyled";
export type ButtonDetails = {
/** Wire member `size`. */
size: ButtonSize,
/** Wire member `circle`. */
circle: boolean,
/** Wire member `classes`. */
classes: Array<string>,
/** Wire member `debounce_ms`. */
debounce_ms: number,
/** Wire member `block_ui`. */
block_ui: boolean,
/** Wire member `icon`. */
icon?: string,
/** Wire member `title`. */
title?: string,
/** Wire member `help`. */
help?: string,
/** Wire member `hotkey`. */
hotkey?: string,
/** Wire member `confirm_title`. */
confirm_title?: string,
/** Wire member `confirm_label`. */
confirm_label?: string,
/** Wire member `cancel_label`. */
cancel_label?: string,
/** Wire member `context`. */
context?: WireDocument, };
export type ButtonSize = "small" | "normal" | "large";
export type ButtonTarget = { "kind": "action",
/**
 * Positive JSON-safe action identity.
 */
id: WireId, } | { "kind": "url",
/**
 * URL text; admission precedes side effects.
 */
href: string, } | { "kind": "inert" };
export type RibbonBackground = "info" | "success" | "warning" | "danger";
export type ScopeKind = "global" | "company";
export type SettingsOptions = {
/** Wire member `scope_kinds`. */
scope_kinds: Array<ScopeKind>,
/** Wire member `app`. */
app: string | null, };
export type WidgetOptions = SettingsOptions | EmptyOptions;
export type ServiceWidget = "web.settings" | "web.addons";
export type PanelVersion = 1;
export type PanelDisplayLimit = number;
export type PanelValue = string | WireId;
export type PanelDomain = {
/** Wire member `model`. */
model: string,
/** Wire member `ast`. */
ast: WireDocument, };
export type PanelSection = { "kind": "category",
/**
 * Positive description-local identity, assigned before pruning.
 */
id: number,
/**
 * Root-model field, independently checked by the host.
 */
field: FieldRef,
/**
 * Bounded localized display label.
 */
label: string,
/**
 * Optional bounded icon token; never HTML or a URL.
 */
icon: string | null,
/**
 * Requested counters before host compatibility derivation, effective counters afterward.
 */
enable_counters: boolean,
/**
 * Whether the range may include values outside the current image.
 */
expand: boolean,
/**
 * Display limit, independent from admission budgets.
 */
limit: PanelDisplayLimit,
/**
 * Caller overlay only; structural callbacks cannot supply defaults.
 */
default_values: Array<PanelValue>,
/**
 * Request the relation's declared hierarchy; selection categories remain flat.
 */
hierarchize: boolean, } | { "kind": "filter",
/**
 * Positive description-local identity, assigned before pruning.
 */
id: number,
/**
 * Root-model field, independently checked by the host.
 */
field: FieldRef,
/**
 * Bounded localized display label.
 */
label: string,
/**
 * Optional bounded icon token; never HTML or a URL.
 */
icon: string | null,
/**
 * Whether counter results are requested.
 */
enable_counters: boolean,
/**
 * Whether the range may include values outside the current image.
 */
expand: boolean,
/**
 * Display limit, independent from admission budgets.
 */
limit: PanelDisplayLimit,
/**
 * Caller overlay only; structural callbacks cannot supply defaults.
 */
default_values: Array<PanelValue>,
/**
 * Scoped AST; criterion fields and category variables have distinct namespaces.
 */
domain: PanelDomain | null,
/**
 * Checked field on the relation's actual comodel.
 */
group_by: FieldRef | null, };
export type ListSearchPanel = { "kind": "sections",
/**
 * Exact supported panel contract version.
 */
version: PanelVersion,
/**
 * Declared applicable view keys; these do not register unavailable view types.
 */
view_types: Array<string>,
/**
 * Ordered caller-authorized sections.
 */
sections: Array<PanelSection>,
/**
 * A bounded fixed compatibility or existing view-selection notice.
 */
notice: string | null, } | { "kind": "selection",
/**
 * Accessible stored selection field.
 */
field: FieldRef,
/**
 * XML string override or the field label.
 */
label: string,
/**
 * Standard enable_counters flag.
 */
enable_counters: boolean,
/**
 * Standard expand flag; false shows only values present in the current base search.
 */
expand: boolean,
/**
 * Context searchpanel_default_FIELD, with false represented as no default.
 */
default_value: string | null,
/**
 * An explicit existing view-selection fallback notice, when present.
 */
notice: string | null, } | { "kind": "unsupported",
/**
 * Precise supported-scope boundary.
 */
message: string, };
export type ViewNode = { "kind": "list",
/**
 * Child arena indexes.
 */
children: Array<number>,
/** Authorized standard search-panel category or explicit unsupported-scope notice. */
search_panel?: ListSearchPanel,
/**
 * Positive XML limit override or the admitted action limit; absent keeps legacy clients.
 */
page_limit?: WirePageLimit,
create?: boolean,
edit?: boolean,
delete?: boolean, } | { "kind": "form",
/**
 * Explicit persistence mode.
 */
record_mode: RecordMode,
/**
 * Child arena indexes.
 */
children: Array<number>,
create?: boolean,
edit?: boolean,
delete?: boolean, } | { "kind": "sheet",
/**
 * Child arena indexes.
 */
children: Array<number>, } | { "kind": "title",
/**
 * Heading and field arena indexes.
 */
children: Array<number>, } | { "kind": "heading",
/**
 * Native heading level (1 or 2).
 */
level: number,
/**
 * Field arena indexes.
 */
children: Array<number>, } | { "kind": "aside",
/**
 * Authorized field/group/separator arena indexes.
 */
children: Array<number>, } | { "kind": "chatter",
/**
 * Reload the parent record after a message is posted.
 */
reload_on_post: boolean,
/**
 * Reload the parent record after an attachment change.
 */
reload_on_attachment: boolean,
/**
 * Reload the parent record after an activity change.
 */
reload_on_activity: boolean,
/**
 * Reload the parent record after a follower change.
 */
reload_on_follower: boolean,
/**
 * Show the attachment panel initially.
 */
open_attachments: boolean,
/**
 * The model inherits the activity mixin; set by the server from the model, never from the arch.
 */
has_activities: boolean, } | { "kind": "header",
/**
 * Button and statusbar field arena indexes.
 */
children: Array<number>, } | { "kind": "button_box",
/**
 * Smart-button arena indexes.
 */
children: Array<number>, } | { "kind": "stat_button",
/**
 * Exposed model method.
 */
name: string,
/**
 * Call kind, currently object only.
 */
button_type: ButtonType,
/**
 * Accessible action caption.
 */
label: string,
/**
 * Closed emphasis vocabulary.
 */
variant: ButtonVariant,
/**
 * Confirmation before calling, if declared.
 */
confirm: string | null,
/**
 * Record-dependent visibility.
 */
modifiers: Modifiers,
/**
 * Optional declarations; absence preserves the legacy payload shape.
 */
details?: ButtonDetails,
/**
 * Readonly statistic field indexes.
 */
children: Array<number>, } | { "kind": "footer",
/**
 * Button arena indexes.
 */
children: Array<number>, } | { "kind": "cancel_button",
/**
 * Caption shown on the button.
 */
label: string,
/**
 * Closed visual emphasis.
 */
variant: ButtonVariant,
/**
 * `invisible` is the only supported expression.
 */
modifiers: Modifiers,
/**
 * Optional declarations; absence preserves the legacy payload shape.
 */
details?: ButtonDetails, } | { "kind": "button",
/**
 * Method name called as `POST /json/2/<model>/<name>`.
 */
name: string,
/**
 * Call kind; only object methods exist in this slice.
 */
button_type: ButtonType,
/**
 * Caption shown on the button.
 */
label: string,
/**
 * Closed visual emphasis.
 */
variant: ButtonVariant,
/**
 * Confirmation question asked before the call, if any.
 */
confirm: string | null,
/**
 * `invisible` is the only supported expression; it sees the record's view fields.
 */
modifiers: Modifiers,
/**
 * Optional declarations; absence preserves the legacy payload shape.
 */
details?: ButtonDetails, } | { "kind": "action_button",
/**
 * Explicit execution target.
 */
target: ButtonTarget,
/**
 * Accessible caption (or a nonempty details title).
 */
label: string,
/**
 * Presentation rank.
 */
variant: ButtonVariant,
/**
 * Optional confirmation question, with legacy required nullable spelling.
 */
confirm: string | null,
/**
 * Visibility expression only.
 */
modifiers: Modifiers,
/**
 * Optional presentation and execution details.
 */
details?: ButtonDetails, } | { "kind": "ribbon",
/**
 * Plain text caption.
 */
text: string,
/**
 * Optional plain text tooltip.
 */
tooltip?: string,
/**
 * Semantic background.
 */
background: RibbonBackground, } | { "kind": "notification_alert", } | { "kind": "group",
/**
 * Optional caption (empty when absent).
 */
label: string,
/**
 * Label/control tracks, two per rendered column.
 */
columns: number,
/**
 * Requested span in label/control tracks, constrained by the client grid.
 */
colspan: number,
/**
 * Child arena indexes.
 */
children: Array<number>, } | { "kind": "notebook",
/**
 * Page arena indexes.
 */
children: Array<number>, } | { "kind": "page",
/**
 * Stable page identity within its notebook.
 */
name: string,
/**
 * Display caption.
 */
label: string,
/**
 * Child arena indexes.
 */
children: Array<number>, } | { "kind": "separator",
/**
 * Display caption.
 */
label: string, } | { "kind": "field",
/**
 * Accessible field handle.
 */
field: FieldRef,
/**
 * Registered widget key.
 */
widget: string,
/**
 * Closed widget-specific options.
 */
options: FieldOptions,
/**
 * Static boolean-literal overrides for this occurrence (`null` inherits model metadata;
 * invisible `null` means false).
 */
modifiers: Modifiers,
/**
 * Display label.
 */
label: string,
/** Whether a readonly value is still written on save; false when omitted. */
force_save: boolean, } | { "kind": "widget",
/**
 * Registered identity.
 */
widget: ServiceWidget,
/**
 * Strict identity-specific options.
 */
options: WidgetOptions,
/**
 * Expression constraints.
 */
modifiers: Modifiers, } | { "kind": "search",
/**
 * Child arena indexes.
 */
children: Array<number>,
/** Authorized panel metadata from this standalone search view. */
search_panel?: ListSearchPanel, } | { "kind": "pivot",
/**
 * Child arena indexes (only `pivot_field`).
 */
children: Array<number>,
disable_linking: boolean,
display_quantity: boolean,
default_order?: string, } | { "kind": "pivot_field",
/**
 * Accessible field handle.
 */
field: FieldRef,
/**
 * Display label.
 */
label: string,
/**
 * What the field does in the table.
 */
role: PivotRole,
/**
 * Date grouping interval: day, week, month, quarter or year (date and datetime row/column fields only).
 */
interval?: string, } | { "kind": "search_field",
/**
 * Accessible field handle.
 */
field: FieldRef,
/**
 * Display label.
 */
label: string,
/**
 * Optional custom filter domain.
 */
filter_domain?: WireDocument,
/**
 * Optional comparison operator.
 */
operator?: string, } | { "kind": "search_filter",
/**
 * Filter identifier.
 */
name: string,
/**
 * Display label.
 */
label: string,
/**
 * Optional domain expression.
 */
domain?: WireDocument,
/**
 * Optional context expression.
 */
context?: WireDocument, } | { "kind": "search_separator", } | { "kind": "search_group",
/**
 * Display caption.
 */
label: string,
/**
 * Standard expand flag; false collapses filters under this group.
 */
expand: boolean,
/**
 * Child arena indexes.
 */
children: Array<number>, };
export type NotificationLevel = "info" | "success" | "warning" | "danger";
export type ActionResult = { "kind": "none", } | { "kind": "current_window",
/**
 * Action title, displayed as text.
 */
name: string,
/**
 * Authorized registered target model.
 */
model: string,
/**
 * List, optionally followed by form; service views are not admitted.
 */
views: Array<ActionView>,
/**
 * Literal base domain. Later ORM requests still apply caller access rules.
 */
domain: unknown[],
/**
 * Bounded literal defaults; authority-bearing context is refused.
 */
context: Record<string, unknown>, } | { "kind": "window",
/**
 * Dialog title.
 */
name: string,
/**
 * Authorized target model.
 */
model: string,
/**
 * Existing record to open, or `null` for a new record.
 */
res_id: WireId | null,
/**
 * One form view request; the producer emits exactly one and clients must refuse any other count.
 */
views: Array<ActionView>,
/**
 * Literal `default_<field>` values only; keys and values are bounded by the producer.
 */
context: Record<string, unknown>, } | { "kind": "close", } | { "kind": "notification",
/**
 * Optional short heading.
 */
title: string | null,
/**
 * Notification text, never markup.
 */
message: string,
/**
 * Closed severity vocabulary.
 */
level: NotificationLevel,
/**
 * Whether the dialog closes after the notification.
 */
then_close: boolean, };
export type ActionResultEnvelope = {
/** Wire member `wire_major`. */
wire_major: WireMajor,
/** Wire member `result`. */
result: ActionResult, };
export type Notice = {
/** Wire member `code`. */
code: string,
/** Wire member `message`. */
message: string, };
export type SettingsScope = { "kind": "global", } | { "kind": "company",
/**
 * Positive checked company identity.
 */
company_id: WireId, };
export type SettingKind = "boolean" | "string" | "integer" | "float" | "selection";
export type SettingValue = boolean | string | number | number;
export type SettingsDescription = {
/** Wire member `wire_major`. */
wire_major: WireMajor,
/** Wire member `registry_version`. */
registry_version: string,
/** Wire member `scope`. */
scope: SettingsScope,
/** Wire member `declarations`. */
declarations: Array<SettingDescription>,
/** Wire member `items`. */
items: Array<SettingsItem>, };
export type SettingsLinkTarget = { "kind": "action",
/**
 * Id of the `ir.actions.act_window` row.
 */
id: WireId,
/**
 * The record the action opens, or `null` to open it as declared.
 */
res_id: WireId | null, } | { "kind": "menu",
/**
 * Id of the `ir.ui.menu` row.
 */
id: WireId, };
export type SettingsItem = { "kind": "info",
/**
 * Addon-qualified identity.
 */
key: string,
/**
 * Display label (the resolver may have chosen a singular or plural form).
 */
label: string,
/**
 * Settings app page.
 */
app: string,
/**
 * Block title within the page.
 */
block: string,
/**
 * Display sort sequence.
 */
sequence: number,
/**
 * Scalar value, displayed as text.
 */
value: SettingValue,
/**
 * Secondary text under the value, lines separated by line feeds, or `null`.
 */
detail: string | null, } | { "kind": "link",
/**
 * Addon-qualified identity.
 */
key: string,
/**
 * Display label.
 */
label: string,
/**
 * Settings app page.
 */
app: string,
/**
 * Block title within the page.
 */
block: string,
/**
 * Display sort sequence.
 */
sequence: number,
/**
 * What the link opens.
 */
target: SettingsLinkTarget, };
export type SettingDescription = {
/** Wire member `key`. */
key: string,
/** Wire member `label`. */
label: string,
/** Wire member `kind`. */
kind: SettingKind,
/** Wire member `default`. */
default: SettingValue,
/** Wire member `choices`. */
choices: Array<[string, string]> | null,
/** Wire member `writable`. */
writable: boolean,
/** Wire member `app`. */
app: string,
/** Wire member `block`. */
block: string,
/** Wire member `sequence`. */
sequence: number, };
export type ValueSource = "default" | "stored";
export type SettingsValues = {
/** Wire member `wire_major`. */
wire_major: WireMajor,
/** Wire member `scope`. */
scope: SettingsScope,
/** Wire member `revision`. */
revision: string,
/** Wire member `values`. */
values: Array<SettingEntry>, };
export type SettingEntry = {
/** Wire member `key`. */
key: string,
/** Wire member `value`. */
value: SettingValue,
/** Wire member `source`. */
source: ValueSource, };
export type AddonState = "installed" | "uninstalled";
export type AddonCatalog = {
/** Wire member `wire_major`. */
wire_major: WireMajor,
/** Wire member `registry_version`. */
registry_version: string,
/** Wire member `addons`. */
addons: Array<AddonEntry>, };
export type AddonEntry = {
/** Wire member `name`. */
name: string,
/** Wire member `label`. */
label: string,
/** Wire member `version`. */
version: string,
/** Wire member `state`. */
state: AddonState,
/** Wire member `installable`. */
installable: boolean,
/** Wire member `summary`. */
summary: string,
/** Wire member `category`. */
category: string,
/** Wire member `author`. */
author: string,
/** Wire member `application`. */
application: boolean,
/** Wire member `website`. */
website: string,
/** Wire member `license`. */
license: string,
/** Wire member `depends`. */
depends: Array<string>, };
export type InstallResult = {
/** Wire member `installed`. */
installed: Array<string>,
/** Wire member `registry_version`. */
registry_version: string,
/** Wire member `asset_version`. */
asset_version: string, };
export type UpgradeResult = {
/** Wire member `upgraded`. */
upgraded: Array<string>,
/** Wire member `installed`. */
installed: Array<string>,
/** Wire member `registry_version`. */
registry_version: string,
/** Wire member `asset_version`. */
asset_version: string, };
export type UninstallResult = {
/** Wire member `uninstalled`. */
uninstalled: Array<string>,
/** Wire member `registry_version`. */
registry_version: string,
/** Wire member `asset_version`. */
asset_version: string, };
export type AddonDetailLimit = number;
export type AddonDetailRequest = {
/** Wire member `name`. */
name: string,
/** Wire member `limit`. */
limit?: AddonDetailLimit,
/** Wire member `dependencies_cursor`. */
dependencies_cursor?: string,
/** Wire member `dependents_cursor`. */
dependents_cursor?: string,
/** Wire member `menus_cursor`. */
menus_cursor?: string,
/** Wire member `views_cursor`. */
views_cursor?: string, };
export type AddonDetailState = "installed" | "uninstalled" | "to_install" | "to_upgrade" | "to_remove" | "uninstallable";
export type AddonNamePage = {
/** Wire member `items`. */
items: Array<string>,
/** Wire member `next_cursor`. */
next_cursor: string | null, };
export type AddonMenu = {
/** Wire member `id`. */
id: WireId,
/** Wire member `name`. */
name: string,
/** Wire member `label`. */
label: string, };
export type AddonView = {
/** Wire member `id`. */
id: WireId,
/** Wire member `name`. */
name: string,
/** Wire member `model`. */
model: string,
/** Wire member `kind`. */
kind: string, };
export type AddonMenuPage = {
/** Wire member `items`. */
items: Array<AddonMenu>,
/** Wire member `next_cursor`. */
next_cursor: string | null, };
export type AddonViewPage = {
/** Wire member `items`. */
items: Array<AddonView>,
/** Wire member `next_cursor`. */
next_cursor: string | null, };
export type AddonDetail = {
/** Wire member `wire_major`. */
wire_major: WireMajor,
/** Wire member `registry_version`. */
registry_version: string,
/** Wire member `name`. */
name: string,
/** Wire member `label`. */
label: string,
/** Wire member `summary`. */
summary: string,
/** Wire member `description`. */
description: string,
/** Wire member `author`. */
author: string,
/** Wire member `available_version`. */
available_version: string | null,
/** Wire member `installed_version`. */
installed_version: string | null,
/** Wire member `state`. */
state: AddonDetailState,
/** Wire member `dependencies`. */
dependencies: AddonNamePage,
/** Wire member `dependents`. */
dependents: AddonNamePage,
/** Wire member `menus`. */
menus: AddonMenuPage,
/** Wire member `views`. */
views: AddonViewPage,
/** Wire member `upgrade_refusal`. */
upgrade_refusal: string | null,
/** Wire member `uninstall_refusal`. */
uninstall_refusal: string | null, };
export type WireDecimalCount = string;
