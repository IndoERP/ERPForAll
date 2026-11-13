/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { readSectionsPanel, type SectionsPanel } from "./panel-metadata.js";
import { PanelCompatibilityError } from "./panel-recovery.js";
import { knownMenuIcon } from "./icons.js";
import { notificationClasses } from "./notification-classes.js";
import type { ButtonRank, ButtonSize } from "./button-control.js";
import type { MessageLevel } from "./message-widgets.js";
import { codeEditorMode } from "./code-editor-session.js";
import { validateSelectionTabField } from "./list-tabs.js";
import { readFieldOptions, type FieldPresentationOptions } from "./field-options.js";
import type { FieldDescription, ButtonDetails as WireButtonDetails, ButtonTarget as WireButtonTarget } from "../generated/human-ui.js";
import { fieldWidgets, serviceControllers } from "./registry.js";
import { ClientError } from "./transport.js";
import { decodeAst, parseBoundedJson, type ValidatedAst } from "../../../../../web/packages/expr/src/index.js";

export function object(value: unknown, label = "response"): Record<string, unknown> {
  if (value === null || typeof value !== "object" || Array.isArray(value)) throw new ClientError("server", `Invalid ${label}.`);
  return value as Record<string, unknown>;
}
function closed(value: Record<string, unknown>, keys: readonly string[], label: string): void {
  for (const key of Object.keys(value)) if (!keys.includes(key)) throw new ClientError("unsupported", `Unknown ${label} member: ${key}.`);
  for (const key of keys) if (!Object.hasOwn(value, key)) throw new ClientError("server", `Missing ${label} member: ${key}.`);
}
export function text(value: unknown, label: string): string {
  if (typeof value !== "string") throw new ClientError("server", `Invalid ${label}.`);
  return value;
}
export function array(value: unknown, label: string): unknown[] {
  if (!Array.isArray(value)) throw new ClientError("server", `Invalid ${label}.`);
  return value;
}
export function integer(value: unknown, label: string): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) throw new ClientError("server", `Invalid ${label}.`);
  return value;
}
function boolean(value: unknown, label: string): boolean {
  if (typeof value !== "boolean") throw new ClientError("server", `Invalid ${label}.`);
  return value;
}
export function unsupported(message: string): never { throw new ClientError("unsupported", message); }
export function noExpression(value: unknown, label: string): void {
  if (value !== null) unsupported(`Dynamic ${label} is not supported by the current human UI slice.`);
}
export interface Field {
  widget?: string; options?: FieldPresentationOptions;
  modifiers?: { readonly: ValidatedAst | null; required: ValidatedAst | null; invisible: ValidatedAst | null };
  name: string; kind: FieldDescription["type"]; label: string; required: boolean; readonly: boolean;
  /** Static view presentation; model metadata defaults to visible and unforced. */
  invisible?: boolean; forceSave?: boolean;
  /** Validated per-occurrence source presentation; no execution semantics. */
  codeMode?: import("./code-editor-types.js").CodeEditorMode;
  /** Admitted selection keys for this header occurrence; current value is always retained. */
  statusbarVisible?: readonly string[] | null;
  /** Statusbar states kept behind the overflow control (never the current one). */
  statusbarFolded?: readonly string[];
  /** The header owner admits direct state changes by click; absent means display-only. */
  statusbarClickable?: boolean;
  selection: readonly (readonly [string, string])[] | null; relation: string | null;
  /** The comodel's inverse many2one of a one2many (`relation_field` on the wire), else null. */
  relationField: string | null;
  /** The comodel views a one2many renders its lines with, attached by the form controller. */
  subview?: Subview;
  /** Why the comodel views could not be attached; the widget shows it in place of the lines. */
  subviewError?: string;
}
/** The embedded list and line form of a one2many, from the comodel's own default views. The
 *  inverse many2one is left out of both: the server sets it from the parent record. */
export interface Subview { model: string; list: readonly Field[]; form: View | undefined; fields: ReadonlyMap<string, Field> }
/** Chatter slot of a record form (addendum): flags are data only; no mail UI exists yet. */
export interface ChatterNode { kind: "chatter"; reloadOnPost: boolean; reloadOnAttachment: boolean; reloadOnActivity: boolean; reloadOnFollower: boolean; openAttachments: boolean; hasActivities: boolean }
export type Node = { kind: "field"; field: Field; label: string; widget: string } | { kind: "group"; label: string; columns: number; colspan: number; children: Node[] }
  | { kind: "widget"; widget: string; options: Record<string, unknown> }
  | { kind: "title"; children: Node[] }
  | { kind: "heading"; level: 1 | 2; children: Node[] }
  | { kind: "root" | "sheet" | "header" | "footer" | "aside" | "button_box"; children: Node[] }
  | ButtonNode | CancelNode
  | {kind: "ribbon"; text: string; tooltip?: string; background: MessageLevel}
  | {kind: "notification_alert"}
  | ChatterNode
  | { kind: "notebook"; children: Node[] }
  | { kind: "page"; name: string; label: string; children: Node[] }
  | { kind: "separator"; label: string }
  | SearchFieldNode | SearchFilterNode | SearchSeparatorNode | SearchGroupNode
  | PivotNode | PivotFieldNode;
/** Pivot root: the two flags of the arch; the grouping and measure fields are its `pivot_field` children. */
export interface PivotNode { kind: "pivot"; children: Node[]; disableLinking: boolean; displayQuantity: boolean; /** `<measure> asc|desc` as the server normalized it. */ defaultOrder: string | null }
/** One field of a pivot: a row grouping, a column grouping or a measure. */
export interface PivotFieldNode { kind: "pivot_field"; field: Field; label: string; role: "row" | "col" | "measure"; /** Date grouping interval of a date or datetime row or column field. */ interval: string | null }
/** Search field filter occurrence. */
export interface SearchFieldNode { kind: "search_field"; field: Field; label: string; filterDomain: ValidatedAst | null; operator: string | null }
/** Predefined search filter or group-by filter. */
export interface SearchFilterNode { kind: "search_filter"; name: string; label: string; domain: ValidatedAst | null; context: ValidatedAst | null }
/** Search separator boundary. */
export interface SearchSeparatorNode { kind: "search_separator" }
/** Group of search filters. */
export interface SearchGroupNode { kind: "search_group"; label: string; expand: boolean; children: Node[] }
/** A header object button: calls `name` on the saved record. `invisible` is an admitted expression over the record's view fields. */
export type ButtonNode = ({ kind: "button" } | { kind: "stat_button"; children: Node[] }) & { name: string; label: string; variant: ButtonRank; confirm: string | null; invisible: ValidatedAst | null; details?: ButtonDetails; target?: ButtonTarget }
/** A footer cancel button: closes the dialog, never saves and names no method. */
export interface CancelNode { kind: "cancel_button"; label: string; variant: ButtonRank; invisible: ValidatedAst | null; details?: ButtonDetails }
export type ListSearchPanel = { kind: "selection"; field: Field; label: string; enableCounters: boolean; expand: boolean; defaultValue: string | null; notice: string | null } | { kind: "unsupported"; message: string } | SectionsPanel;
export interface View {
  type: "list" | "form" | "search" | "pivot";
  mode: "record" | "service";
  nodes: Node[];
  fields: Field[];
  searchPanel?: ListSearchPanel;
  pageLimit?: number;
  canCreate?: boolean;
  canEdit?: boolean;
  canDelete?: boolean;
}
export interface Description { model: string; fields: Map<string, Field>; views: Map<string, View> }
/** The combined record policy for repeated field occurrences in one view. */
export function combineFields(fields: readonly Field[]): Field[] {
  const combined = new Map<string, Field>();
  for (const field of fields) {
    const previous = combined.get(field.name);
    combined.set(field.name, previous ? { ...(previous.invisible && !field.invisible ? field : previous), readonly: previous.readonly && field.readonly,
      required: previous.required || field.required, invisible: Boolean(previous.invisible && field.invisible),
      forceSave: Boolean(previous.forceSave || field.forceSave) } : { ...field });
  }
  return [...combined.values()];
}

/** Admitted client behavior derived from the Rust-owned details DTO. */
export interface ButtonDetails extends Omit<WireButtonDetails,"context"> { context?: ValidatedAst }
export type ButtonTarget = WireButtonTarget;
const buttonRanks: readonly string[] = ["primary","secondary","link","success","info","warning","danger","unstyled"];
function buttonRank(value: unknown): ButtonRank {
  if(typeof value !== "string" || !buttonRanks.includes(value)) unsupported("Unsupported button variant.");
  return value as ButtonRank;
}
function boundedButtonText(value: unknown, label: string, max=4096): string {
  const result=text(value,label);
  if(result.length > max || new TextEncoder().encode(result).length > max) throw new ClientError("server", `Button ${label} exceeds ${max} UTF-8 bytes.`);
  return result;
}
function buttonConfirmation(value: unknown): string | null {
  if(value===null)return null;
  const result=boundedButtonText(value,"confirmation");
  if(!result.trim())throw new ClientError("server","Button confirmation must be nonempty.");
  return result;
}
function buttonDetails(value: unknown, admitContext: (raw:unknown)=>ValidatedAst): ButtonDetails {
  const details=object(value,"button details"), optional=["icon","title","help","hotkey","confirm_title","confirm_label","cancel_label","context"];
  closed(details,["size","circle","classes","debounce_ms","block_ui",...optional.filter(key=>Object.hasOwn(details,key))],"button details");
  if(typeof details["size"]!=="string" || !["small","normal","large"].includes(details["size"])) throw new ClientError("server","Invalid button size.");
  if(typeof details["circle"]!=="boolean") throw new ClientError("server","Button circle must be boolean.");
  if(typeof details["block_ui"]!=="boolean") throw new ClientError("server","Button block_ui must be boolean.");
  const interval=details["debounce_ms"];
  if(typeof interval!=="number" || !Number.isSafeInteger(interval) || interval<0 || interval>60_000) throw new ClientError("server","Button debounce must be an integer from 0 to 60000.");
  const classes=array(details["classes"],"button classes");
  if(classes.length>8) throw new ClientError("server","Button classes must contain at most 8 tokens.");
  const classText=classes.map(value=>boundedButtonText(value,"class token",64)).join(" ");
  let canonical:string;
  try { canonical=notificationClasses(classText,"notification"); }
  catch(error) { if(error instanceof ClientError) throw new ClientError("server",error.message.replace("notification className","button classes")); throw error; }
  if(classes.some(value=>typeof value!=="string" || !value || value.includes(" "))) throw new ClientError("server","Button classes require nonempty identifier tokens.");
  const result:ButtonDetails={size:details["size"] as ButtonSize,circle:details["circle"],classes:canonical?canonical.split(" "):[],debounce_ms:interval,block_ui:details["block_ui"]};
  for(const key of ["icon","title","help","hotkey","confirm_title","confirm_label","cancel_label"] as const) if(Object.hasOwn(details,key)) result[key]=boundedButtonText(details[key],key,key==="icon"?256:key==="hotkey"?8:4096);
  for(const key of ["confirm_title","confirm_label","cancel_label"] as const) if(result[key]!==undefined && !result[key]!.trim())throw new ClientError("server",`Button ${key} must be nonempty.`);
  if(result.icon!==undefined && knownMenuIcon(result.icon)===null) throw new ClientError("server",`Unknown button icon: ${result.icon}.`);
  if(result.hotkey!==undefined && !/^(?:shift\+)?[a-z0-9]$/.test(result.hotkey)) throw new ClientError("server","Invalid button hotkey.");
  if(Object.hasOwn(details,"context")) result.context=admitContext(details["context"]);
  return result;
}

function staticModifier(value: unknown, label: string, inherited: boolean): boolean {
  if (value === null) return inherited;
  const ast = decodeAst(parseBoundedJson(JSON.stringify(value)));
  const node = ast.nodes[0];
  if (ast.nodes.length !== 1 || node?.kind !== "literal" || node.value.kind !== "bool") {
    if (node?.kind === "literal") return unsupported(`Dynamic ${label} modifiers is not supported by the current human UI slice.`);
    return inherited; // Dynamic policy is evaluated by RecordForm from this record snapshot.
  }
  return node.value.value;
}
const kinds = new Set(["char", "text", "html", "boolean", "integer", "float", "monetary", "date", "datetime", "selection", "many2one", "one2many", "many2many"]);
export function describeViews(raw: unknown, model: string, registryVersion: string): Description {
  const data = object(raw, "view description");
  let contextBytes=0, contextNodes=0, contextCount=0;
  const admitContext=(raw:unknown):ValidatedAst=>{
    const encoded=JSON.stringify(raw), bytes=new TextEncoder().encode(encoded).length;
    const source=object(raw,"button context"), nodes=array(source["nodes"],"button context nodes");
    if(++contextCount>1024 || contextBytes+bytes>16*1024*1024 || contextNodes+nodes.length>262144) throw new ClientError("server","Button contexts exceed the description expression allowance.");
    contextBytes+=bytes;contextNodes+=nodes.length;
    return decodeAst(parseBoundedJson(encoded));
  };
  closed(data, ["wire_major", "registry_version", "description_version", "model", "fields", "views", "notices"], "view description");
  if (data["wire_major"] !== 1 || data["registry_version"] !== registryVersion) throw new ClientError("unsupported", "The application changed. Reload to use the current version.");
  if (data["model"] !== model) throw new ClientError("server", "View model does not match the action.");
  text(data["description_version"], "description version");
  if (array(data["notices"], "notices").length) unsupported("This description contains notices that the current human UI slice cannot interpret.");
  const fields = new Map<string, Field>();
  for (const [name, value] of Object.entries(object(object(data["fields"])[model], "fields"))) {
    const f = object(value, "field");
    closed(f, ["type", "label", "required", "readonly", "selection", "relation", "relation_field"], "field description");
    const kind = text(f["type"], "field type");
    if (!/^[a-zA-Z_][a-zA-Z0-9_]*$/.test(name) || !kinds.has(kind)) unsupported(`Unsupported field: ${name} (${kind}).`);
    const selection = f["selection"] === null ? null : array(f["selection"], "selection").map((pair): readonly [string, string] => {
      const values = array(pair, "selection choice");
      if (values.length !== 2) throw new ClientError("server", "Invalid selection choice.");
      return [text(values[0], "selection key"), text(values[1], "selection label")];
    });
    const relation = f["relation"] === null ? null : text(f["relation"], "relation");
    const relationField = f["relation_field"] === null ? null : text(f["relation_field"], "relation field");
    if (kind === "selection" && !selection || (kind === "many2one" || kind === "one2many" || kind === "many2many") && !relation
      || (kind === "one2many") !== (relationField !== null)) throw new ClientError("server", `Incomplete field metadata: ${name}.`);
    fields.set(name, { name, kind: kind as FieldDescription["type"], label: text(f["label"], "field label"), required: boolean(f["required"], "required"), readonly: boolean(f["readonly"], "readonly"), selection, relation, relationField });
  }
  const views = new Map<string, View>();
  let totalNodes = 0;
  for (const rawView of array(data["views"], "views")) {
    const v = object(rawView), type = v["type"], renderer = text(v["renderer"], "renderer");
    closed(v, ["identity", "type", "schema_major", "renderer", "root", "nodes"], "view");
    const identity = object(v["identity"], "view identity");
    if (identity["kind"] === "stored") { closed(identity, ["kind", "id"], "view identity"); if (integer(identity["id"], "stored view identity") === 0) throw new ClientError("server", "Invalid stored view identity."); }
    else if (identity["kind"] === "generated") closed(identity, ["kind"], "view identity");
    else unsupported("Unknown view identity kind.");
    const sectionRenderer=(type==="list"&&renderer==="web.list_sections_v1")||(type==="search"&&renderer==="web.search_sections_v1");
    if ((type==="list"||type==="search") && renderer.startsWith(`web.${type}_sections_`) && !sectionRenderer) throw new PanelCompatibilityError();
    if ((type !== "list" && type !== "form" && type !== "search" && type !== "pivot") || (!sectionRenderer&&renderer !== `web.${type}`) || v["schema_major"] !== 1) unsupported(`Unsupported view renderer: ${renderer}.`);
    if (views.has(type)) throw new ClientError("server", "Duplicate view type.");
    const arena = array(v["nodes"], "view nodes"), root = integer(v["root"], "view root");
    if (!arena.length || arena.length > 4096) throw new ClientError("server", "View node limit exceeded.");
    totalNodes += arena.length;
    if (totalNodes > 4096) throw new ClientError("server", "Requested views exceed the 4096-node aggregate limit.");
    const rootNode = object(arena[root], "view root");
    if (rootNode["kind"] !== type) throw new ClientError("server", "View root type does not match renderer.");
    const mode = (type === "list" || type === "search" || type === "pivot") ? "record" : rootNode["record_mode"];
    if (mode !== "record" && mode !== "service" || (type === "list" || type === "search" || type === "pivot") && mode !== "record") unsupported("Unsupported form record mode.");
    const seen = new Set<number>(), selected: Field[] = [], contextFields = new Set<string>();
    const admitViewContext = (raw: unknown): ValidatedAst => {
      const ast = admitContext(raw);
      for (const node of ast.nodes) if (node.kind === "name" && fields.has(node.name)) contextFields.add(node.name);
      return ast;
    };
    const visit = (index: number, depth: number, parentKind?: unknown): Node => {
      if (seen.has(index)) throw new ClientError("server", "View structure contains a cycle or repeated node.");
      if (depth > 32) throw new ClientError("server", "View nesting limit exceeded.");
      seen.add(index);
      const node = object(arena[index], "view node"), kind = node["kind"];
      if (kind === "form") closed(node, ["kind", "record_mode", "children", ...(Object.hasOwn(node, "create") ? ["create"] : []), ...(Object.hasOwn(node, "edit") ? ["edit"] : []), ...(Object.hasOwn(node, "delete") ? ["delete"] : [])], "view node");
      else if (kind === "list") closed(node, ["kind", "children", ...(Object.hasOwn(node, "search_panel") ? ["search_panel"] : []), ...(Object.hasOwn(node, "page_limit") ? ["page_limit"] : []), ...(Object.hasOwn(node, "create") ? ["create"] : []), ...(Object.hasOwn(node, "edit") ? ["edit"] : []), ...(Object.hasOwn(node, "delete") ? ["delete"] : [])], "view node");
      else if (kind === "field") closed(node, ["kind", "field", "widget", "options", "modifiers", "label", "force_save"], "view node");
      else if (kind === "widget") closed(node, ["kind", "widget", "options", "modifiers"], "view node");
      else if (kind === "button" || kind === "stat_button") closed(node, ["kind", "name", "button_type", "label", "variant", "confirm", "modifiers", ...(Object.hasOwn(node,"details")?["details"]:[]), ...(kind === "stat_button" ? ["children"] : [])], "view node");
      else if (kind === "cancel_button") closed(node, ["kind", "label", "variant", "modifiers",...(Object.hasOwn(node,"details")?["details"]:[])], "view node");
      else if(kind === "action_button") closed(node,["kind","target","label","variant","confirm","modifiers",...(Object.hasOwn(node,"details")?["details"]:[])],"view node");
      else if(kind === "ribbon") closed(node,["kind","text","background",...(Object.hasOwn(node,"tooltip")?["tooltip"]:[])],"view node");
      else if(kind === "notification_alert") closed(node,["kind"],"view node");
      else if(kind === "chatter") closed(node,["kind","reload_on_post","reload_on_attachment","reload_on_activity","reload_on_follower","open_attachments","has_activities"],"view node");
      else if (kind === "sheet" || kind === "header" || kind === "footer" || kind === "aside" || kind === "title" || kind === "notebook" || kind === "button_box") closed(node, ["kind", "children"], "view node");
      else if (kind === "heading") closed(node, ["kind", "level", "children"], "view node");
      else if (kind === "group") closed(node, ["kind", "label", "columns", "colspan", "children"], "view node");
      else if (kind === "page") closed(node, ["kind", "name", "label", "children"], "view node");
      else if (kind === "separator") closed(node, ["kind", "label"], "view node");
      else if (kind === "pivot") closed(node, ["kind", "children", "disable_linking", "display_quantity", ...(Object.hasOwn(node, "default_order") ? ["default_order"] : [])], "view node");
      else if (kind === "pivot_field") closed(node, ["kind", "field", "label", "role", ...(Object.hasOwn(node, "interval") ? ["interval"] : [])], "view node");
      else if (kind === "search") closed(node, ["kind", "children", ...(Object.hasOwn(node, "search_panel") ? ["search_panel"] : [])], "view node");
      else if (kind === "search_field") closed(node, ["kind", "field", "label", ...(Object.hasOwn(node, "filter_domain") ? ["filter_domain"] : []), ...(Object.hasOwn(node, "operator") ? ["operator"] : [])], "view node");
      else if (kind === "search_filter") closed(node, ["kind", "name", "label", ...(Object.hasOwn(node, "domain") ? ["domain"] : []), ...(Object.hasOwn(node, "context") ? ["context"] : [])], "view node");
      else if (kind === "search_separator") closed(node, ["kind"], "view node");
      else if (kind === "search_group") closed(node, ["kind", "label", ...(Object.hasOwn(node, "expand") ? ["expand"] : []), "children"], "view node");
      else unsupported(`Unsupported view node: ${String(kind)}.`);
      if (depth > 1 && (kind === "form" || kind === "list" || kind === "search" || kind === "pivot") || type === "pivot" && kind !== "pivot" && kind !== "pivot_field"
        || type !== "pivot" && (kind === "pivot" || kind === "pivot_field") || kind === "pivot_field" && parentKind !== "pivot"
        || type === "list" && kind !== "list" && kind !== "field"
        || type === "search" && kind !== "search" && kind !== "search_field" && kind !== "search_filter" && kind !== "search_separator" && kind !== "search_group"
        || type !== "search" && (kind === "search" || kind === "search_field" || kind === "search_filter" || kind === "search_separator" || kind === "search_group")
        || parentKind === "search_group" && kind !== "search_filter" && kind !== "search_separator"
        || parentKind === "notebook" && kind !== "page" || kind === "page" && parentKind !== "notebook"
        || kind === "sheet" && parentKind !== "form"
        || kind === "aside" && (parentKind !== "form" || mode !== "record")
        || kind === "chatter" && (parentKind !== "form" || mode !== "record")
        || kind === "title" && (!['form', 'sheet', 'group', 'page'].includes(String(parentKind)) || mode !== "record")
        || kind === "heading" && parentKind !== "title"
        || parentKind === "title" && kind !== "heading" && kind !== "field"
        || parentKind === "heading" && kind !== "field"
        || parentKind === "aside" && kind !== "field" && kind !== "group" && kind !== "separator" && kind !== "ribbon" && kind !== "notification_alert"
        || kind === "button_box" && parentKind !== "sheet"
        || kind === "stat_button" && parentKind !== "button_box"
        || parentKind === "button_box" && kind !== "stat_button"
        || parentKind === "stat_button" && kind !== "field"
        || kind === "header" && (parentKind !== "form" || mode !== "record")
        || kind === "footer" && (parentKind !== "form" || mode !== "record")
        || parentKind === "header" && kind !== "button" && kind !== "action_button" && kind !== "field"
        || parentKind === "footer" && kind !== "button" && kind !== "action_button" && kind !== "cancel_button"
        || (kind === "button" || kind === "action_button") && parentKind !== "header" && parentKind !== "footer"
        || (kind === "ribbon" || kind === "notification_alert") && (mode !== "record" || typeof parentKind!=="string" || !["form","sheet","group","page","aside"].includes(parentKind))
        || kind === "cancel_button" && parentKind !== "footer"
        || mode === "service" && kind !== "form" && kind !== "widget") {
        throw new ClientError("server", "Invalid form layout hierarchy.");
      }
      if (node["modifiers"] !== undefined) {
        const mods = object(node["modifiers"], "modifiers");
        closed(mods, ["readonly", "required", "invisible"], "modifiers");
        // Only a header button reads `invisible`; every other node keeps the static-only contract.
        for (const key of ["readonly", "required", "invisible"]) if (kind !== "field" && (kind !== "button" && kind !== "action_button" && kind !== "stat_button" && kind !== "cancel_button" || key !== "invisible")) noExpression(mods[key], `${key} modifiers`);
        if (Object.keys(mods).some(key => !["readonly", "required", "invisible"].includes(key))) unsupported("Unsupported modifier.");
      }
      if (kind === "field") {
        if (mode !== "record") unsupported("Service forms cannot contain record fields.");
        const ref = object(node["field"]); closed(ref, ["model", "name"], "field reference");
        const name = text(ref["name"], "field name"), field = fields.get(name);
        if (!field || ref["model"] !== model) throw new ClientError("server", `Unknown field in view: ${name}.`);
        const widget = text(node["widget"], "field widget");
        const statistic = parentKind === "stat_button";
        const relationCount = widget === "relation_count";
        if (statistic && !(widget === "statinfo" && (field.kind === "integer" || field.kind === "float") || relationCount && field.kind === "one2many")) unsupported("Smart-button statistics require numeric statinfo or one2many relation_count fields.");
        if (relationCount && !statistic) unsupported("relation_count requires a smart button.");
        if (!statistic && widget !== field.kind && (!fieldWidgets.has(widget) || !fieldWidgets.get(widget).kinds.includes(field.kind))) unsupported(`Unsupported field widget or options: ${widget}.`);
        const options = object(node["options"], "field options");
        let statusbarVisible: readonly string[] | null | undefined, statusbarFolded: readonly string[] = [], statusbarClickable = false;
        if (widget === "statusbar") {
          if (parentKind !== "header" || field.kind !== "selection") unsupported("The statusbar widget requires a selection field in a record form header.");
          // `folded` and `clickable` are optional additions to the closed `visible` contract (display-only when absent).
          const { folded, clickable, ...contract } = options;
          closed(contract, ["visible"], "statusbar options");
          statusbarFolded = folded === undefined ? [] : array(folded, "statusbar folded keys").map(key => text(key, "statusbar folded key"));
          if (new Set(statusbarFolded).size !== statusbarFolded.length || statusbarFolded.some(key => !field.selection?.some(([value]) => value === key))) unsupported("Invalid statusbar folded keys.");
          statusbarClickable = clickable === undefined ? false : boolean(clickable, "statusbar clickable");
          statusbarVisible = options["visible"] === null ? null : array(options["visible"], "statusbar visible keys").map(key => text(key, "statusbar visible key"));
          if (statusbarVisible !== null && (!statusbarVisible.length || new Set(statusbarVisible).size !== statusbarVisible.length || statusbarVisible.some(key => !field.selection?.some(([value]) => value === key)))) unsupported("Invalid statusbar visible keys.");
        } else if (widget === "code") {
          if (parentKind === "header" || field.kind !== "text") unsupported("The code widget requires a text field outside the form header.");
          closed(options, ["mode"], "code field options");
          codeEditorMode(options["mode"]);
        } else {
          if (parentKind === "header") unsupported("Header fields require the statusbar widget.");
          readFieldOptions(options, model, fields, field.relation);
        }
        const mods = object(node["modifiers"], "modifiers");
        const occurrence: Field = { ...field, widget, ...(widget === "statusbar" || widget === "code" ? {} : { options: readFieldOptions(options, model, fields, field.relation) }), label: text(node["label"], "field label"),
          readonly: staticModifier(mods["readonly"], "readonly", false) || field.readonly || name === "id",
          required: staticModifier(mods["required"], "required", field.required),
          invisible: staticModifier(mods["invisible"], "invisible", false),
          forceSave: boolean(node["force_save"], "force_save"),
          modifiers: { readonly: mods["readonly"] === null ? null : decodeAst(parseBoundedJson(JSON.stringify(mods["readonly"]))), required: mods["required"] === null ? null : decodeAst(parseBoundedJson(JSON.stringify(mods["required"]))), invisible: mods["invisible"] === null ? null : decodeAst(parseBoundedJson(JSON.stringify(mods["invisible"]))) },
          ...(widget === "statusbar" ? { statusbarVisible, statusbarFolded, statusbarClickable } : {}),
          ...(widget === "code" ? { codeMode: codeEditorMode(options["mode"]) } : {}) };
        if (statistic && (!occurrence.readonly || occurrence.forceSave)) throw new ClientError("server", "Smart-button statistics must be readonly.");
        // The count provider calls COUNT; loading this O2M into state would materialize ids.
        // An ordinary occurrence of the same relation is still selected by its own node.
        if (!relationCount) selected.push(occurrence);
        for (const key of ["placeholder_field", "currency_field", "start_date_field", "end_date_field", "whitelist_fname", "color_field"] as const) {
          const reference = occurrence.options?.[key];
          if (reference && reference.model === model) {
            const dependency = fields.get(reference.name);
            if (!dependency) throw new ClientError("server", `Missing field dependency: ${reference.name}.`);
            // Range endpoints are editable participants. Other dependencies are read inputs only.
            selected.push({ ...dependency, readonly: key === "start_date_field" || key === "end_date_field" ? dependency.readonly : true, invisible: true, forceSave: false });
          }
        }

        // The host has authorized AST references too; request their values even without a visible occurrence.
        const documents: (ValidatedAst | null)[] = Object.values(occurrence.modifiers ?? {});
        for (const document of [occurrence.options?.domain, occurrence.options?.context]) if (document) documents.push(decodeAst(parseBoundedJson(JSON.stringify(document))));
        for (const ast of documents) {
          if (!ast) continue;
          for (const item of ast.nodes) if (item.kind === "name") {
            const dependency = fields.get(item.name);
            if (dependency) selected.push({ ...dependency, readonly: true, invisible: true, forceSave: false });
          }
        }

        return { kind: "field", field: occurrence, widget, label: occurrence.label };
      }
      if (kind === "widget") {
        const widget = text(node["widget"], "service widget"), options = object(node["options"]);
        if (mode !== "service") unsupported("Record forms cannot contain service widgets.");
        const controller = serviceControllers.get(widget);
        if (controller.model !== model) unsupported("Service widget does not match its model.");
        controller.validateOptions(options);
        return { kind: "widget", widget, options };
      }
      if (kind === "separator") return { kind, label: text(node["label"], "separator label") };
      if(kind === "ribbon") {
        const background=node["background"];
        if(typeof background!=="string" || !["info","success","warning","danger"].includes(background)) throw new ClientError("server","Invalid ribbon background.");
        return {kind,text:boundedButtonText(node["text"],"ribbon text"),background:background as MessageLevel,...(Object.hasOwn(node,"tooltip")?{tooltip:boundedButtonText(node["tooltip"],"ribbon tooltip")}:{})};
      }
      if(kind === "notification_alert") return {kind};
      if(kind === "chatter") return {kind,reloadOnPost:boolean(node["reload_on_post"],"chatter reload_on_post"),reloadOnAttachment:boolean(node["reload_on_attachment"],"chatter reload_on_attachment"),reloadOnActivity:boolean(node["reload_on_activity"],"chatter reload_on_activity"),reloadOnFollower:boolean(node["reload_on_follower"],"chatter reload_on_follower"),openAttachments:boolean(node["open_attachments"],"chatter open_attachments"),hasActivities:boolean(node["has_activities"],"chatter has_activities")};
      if(kind === "action_button") {
        const raw=object(node["target"],"button target"), targetKind=raw["kind"];
        let target:ButtonTarget;
        if(targetKind === "action") {closed(raw,["kind","id"],"button target");target={kind:"action",id:integer(raw["id"],"button action id")};if(target.id<=0)throw new ClientError("server","Invalid button action id.");}
        else if(targetKind === "url") {closed(raw,["kind","href"],"button target");const href=boundedButtonText(raw["href"],"URL");if(/[\\\s\u0000-\u001f\u007f]/.test(href)||!(href.startsWith("/")&&!href.startsWith("//")||/^https?:\/\//i.test(href)))throw new ClientError("server","Invalid button URL.");let url:URL;try{url=new URL(href,"https://erp.invalid/");}catch{throw new ClientError("server","Invalid button URL.");}if(!href.trim() || !["http:","https:"].includes(url.protocol) || url.username || url.password)throw new ClientError("server","Invalid button URL.");target={kind:"url",href};}
        else if(targetKind === "inert") {closed(raw,["kind"],"button target");target={kind:"inert"};}
        else throw new ClientError("server","Invalid button target.");
        const details=Object.hasOwn(node,"details")?buttonDetails(node["details"],admitViewContext):undefined;
        const label=boundedButtonText(node["label"],"label"), confirm=buttonConfirmation(node["confirm"]);
        if(!label.trim() && !details?.title?.trim()) throw new ClientError("server","Invalid button name or label.");
        if(confirm===null && details && [details.confirm_title,details.confirm_label,details.cancel_label].some(value=>value!==undefined)) throw new ClientError("server","Button confirmation labels require confirm.");
        const rawInvisible=object(node["modifiers"],"modifiers")["invisible"];
        return {kind:"button",target,name:target.kind==="action"?`action:${target.id}`:target.kind==="url"?`url:${target.href}`:"inert",label,variant:buttonRank(node["variant"]),confirm,invisible:rawInvisible===null?null:decodeAst(parseBoundedJson(JSON.stringify(rawInvisible))),...(details?{details}:{})};
      }
      if (kind === "cancel_button") {
        const label = boundedButtonText(node["label"], "label"), variant = node["variant"];
        const details=Object.hasOwn(node,"details")?buttonDetails(node["details"],admitViewContext):undefined;
        if (!label.trim() && !details?.title?.trim()) throw new ClientError("server", "Invalid button name or label.");
        buttonRank(variant);
        const rawInvisible = object(node["modifiers"], "modifiers")["invisible"];
        if(details?.context || details && [details.confirm_title,details.confirm_label,details.cancel_label].some(value=>value!==undefined))throw new ClientError("server","Cancel buttons cannot declare context or confirmation labels.");
        return { kind, label, variant:buttonRank(variant), ...(details?{details}:{}), invisible: rawInvisible === null ? null : decodeAst(parseBoundedJson(JSON.stringify(rawInvisible))) };
      }
      if (kind === "button" || kind === "stat_button") {
        const name = text(node["name"], "button name"), label = text(node["label"], "button label"), variant = node["variant"];
        if (node["button_type"] !== "object") unsupported("Only object buttons are supported.");
        if (name.length>128 || !/^[a-z][a-z0-9_]*$/.test(name) || !label.trim() && !(Object.hasOwn(node,"details") && typeof object(node["details"])["title"]==="string" && String(object(node["details"])["title"]).trim())) throw new ClientError("server", "Invalid button name or label.");
        buttonRank(variant);
        const confirm = buttonConfirmation(node["confirm"]);
        const rawInvisible = object(node["modifiers"], "modifiers")["invisible"];
        // The metadata cache hands out structured clones, which no longer carry the bounded reader's
        // provenance; the expression is re-read through the same bounded reader (each with its own admission budget) before it is admitted.
        const invisible = rawInvisible === null ? null : decodeAst(parseBoundedJson(JSON.stringify(rawInvisible)));
        const details=Object.hasOwn(node,"details")?buttonDetails(node["details"],admitViewContext):undefined;
        if(confirm===null && details && [details.confirm_title,details.confirm_label,details.cancel_label].some(value=>value!==undefined)) throw new ClientError("server","Button confirmation labels require confirm.");
        const action: Pick<ButtonNode, "name" | "label" | "variant" | "confirm" | "invisible" | "details"> = { name, label:boundedButtonText(label,"label"), variant:buttonRank(variant), confirm:confirm===null?null:boundedButtonText(confirm,"confirmation"), invisible,...(details?{details}:{}) };
        if (kind === "stat_button") return { kind, ...action, children: array(node["children"], "statistic fields").map(child => visit(integer(child, "child index"), depth + 1, kind)) };
        return { kind, ...action };
      }
      if (kind === "pivot_field") {
        const ref = object(node["field"]); closed(ref, ["model", "name"], "field reference");
        const name = text(ref["name"], "field name"), field = fields.get(name);
        if (!field || ref["model"] !== model) throw new ClientError("server", `Unknown field in view: ${name}.`);
        const role = node["role"];
        if (role !== "row" && role !== "col" && role !== "measure") unsupported("Unsupported pivot field role.");
        const label = text(node["label"], "field label");
        selected.push({ ...field, label, readonly: true, required: false, invisible: false, forceSave: false });
        const interval = Object.hasOwn(node, "interval") ? text(node["interval"], "pivot interval") : null;
        if (interval !== null && !["day", "week", "month", "quarter", "year"].includes(interval)) unsupported("Unsupported pivot interval.");
        return { kind: "pivot_field", field, label, role: role as "row" | "col" | "measure", interval };
      }
      if (kind === "search_field") {
        const ref = object(node["field"]); closed(ref, ["model", "name"], "field reference");
        const name = text(ref["name"], "field name"), field = fields.get(name);
        if (!field || ref["model"] !== model) throw new ClientError("server", `Unknown field in view: ${name}.`);
        const label = text(node["label"], "field label");
        const filterDomain = Object.hasOwn(node, "filter_domain") && node["filter_domain"] !== null ? decodeAst(parseBoundedJson(JSON.stringify(node["filter_domain"]))) : null;
        const operator = Object.hasOwn(node, "operator") && node["operator"] !== null ? text(node["operator"], "search field operator") : null;
        if (operator !== null && !["ilike", "=", "!=", ">", "<", ">=", "<="].includes(operator)) unsupported(`Unsupported search field operator '${operator}' for '${model}.${name}'.`);
        return { kind: "search_field", field, label, filterDomain, operator };
      }
      if (kind === "search_filter") {
        const name = text(node["name"], "filter name"), label = text(node["label"], "filter label");
        if (!name.trim() || !label.trim()) throw new ClientError("server", "Search filter requires name and label.");
        const filterDom = Object.hasOwn(node, "domain") && node["domain"] !== null ? decodeAst(parseBoundedJson(JSON.stringify(node["domain"]))) : null;
        const filterCtx = Object.hasOwn(node, "context") && node["context"] !== null ? decodeAst(parseBoundedJson(JSON.stringify(node["context"]))) : null;
        return { kind: "search_filter", name, label, domain: filterDom, context: filterCtx };
      }
      if (kind === "search_separator") {
        return { kind: "search_separator" };
      }
      const children = array(node["children"], "children").map(child => visit(integer(child, "child index"), depth + 1, kind));
      if (kind === "heading") {
        const level = integer(node["level"], "heading level");
        if (level !== 1 && level !== 2) unsupported("Only title heading levels 1 and 2 are supported.");
        return { kind, level, children };
      }
      if (kind === "search_group") {
        const expand = Object.hasOwn(node, "expand") ? boolean(node["expand"], "search group expand") : false;
        return { kind: "search_group", label: text(node["label"], "search group label"), expand, children };
      }
      if (kind === "search") {
        return { kind: "root", children };
      }
      if (kind === "pivot") return { kind, children, disableLinking: boolean(node["disable_linking"], "pivot disable_linking"), displayQuantity: boolean(node["display_quantity"], "pivot display_quantity"), defaultOrder: Object.hasOwn(node, "default_order") ? text(node["default_order"], "pivot default_order") : null };
      if (kind === "title") return { kind, children };
      if (kind === "group") {
        const columns = integer(node["columns"], "group columns"), colspan = integer(node["colspan"], "group colspan");
        if ([columns, colspan].some(n => n < 2 || n > 12 || n % 2 !== 0)) throw new ClientError("server", "Unsupported group columns or colspan.");
        return { kind, label: text(node["label"], "group label"), columns, colspan, children };
      }
      if (kind === "page") {
        const name = text(node["name"], "page name"), label = text(node["label"], "page label");
        if (!name.trim() || !label.trim()) throw new ClientError("server", "Notebook pages require a name and label.");
        return { kind, name, label, children };
      }
      if (kind === "notebook") {
        const names = children.map(child => child.kind === "page" ? child.name : "");
        if (new Set(names).size !== names.length) throw new ClientError("server", "Duplicate notebook page name.");
        return { kind, children };
      }
      if (kind === "footer" && children.filter(child => child.kind === "cancel_button").length > 1) throw new ClientError("server", "A footer allows at most one cancel button.");
      if (kind === "sheet" || kind === "header" || kind === "footer" || kind === "aside" || kind === "button_box") return { kind, children };
      if (children.filter(child => child.kind === "aside").length > 1) throw new ClientError("server", "A record form allows at most one aside.");
      if (children.filter(child => child.kind === "chatter").length > 1) throw new ClientError("server", "A record form allows at most one chatter.");
      if (children.some((child, position) => child.kind === "header" && position !== 0 || child.kind === "footer" && position !== children.length - 1)) throw new ClientError("server", "Invalid form layout hierarchy.");
      return { kind: "root", children };
    };
    let searchPanel: ListSearchPanel | undefined;
    if ((type === "list" || type === "search") && rootNode["search_panel"] !== undefined) {
      const panel = object(rootNode["search_panel"], "search panel");
      if(panel["kind"]==="sections") {
        if(!sectionRenderer)throw new PanelCompatibilityError();
        searchPanel=readSectionsPanel(panel,model,fields,admitContext);
        if(type==="list"&&!searchPanel.viewTypes.includes("list"))throw new ClientError("server","Search panel does not apply to this view.");
      } else if (panel["kind"] === "unsupported") {
        closed(panel, ["kind", "message"], "search panel");
        const message = text(panel["message"], "search panel message");
        if (!message.trim() || message.length > 4096) throw new ClientError("server", "Invalid search panel message.");
        searchPanel = { kind: "unsupported", message };
      } else if (panel["kind"] === "selection") {
        closed(panel, ["kind", "field", "label", "enable_counters", "expand", "default_value", "notice"], "search panel");
        const reference = object(panel["field"], "search panel field");
        closed(reference, ["model", "name"], "search panel field");
        const name = text(reference["name"], "search panel field name"), field = fields.get(name);
        if (!field || reference["model"] !== model) throw new ClientError("server", `Unknown search panel field: ${name}.`);
        validateSelectionTabField(field);
        const label = text(panel["label"], "search panel label");
        if (!label.trim() || new TextEncoder().encode(label).length > 256) throw new ClientError("server", "Invalid search panel label.");
        const defaultValue = panel["default_value"] === null ? null : text(panel["default_value"], "search panel default");
        if (defaultValue !== null && !field.selection!.some(([key]) => key === defaultValue)) throw new ClientError("server", "Invalid search panel default selection.");
        const notice = panel["notice"] === null ? null : text(panel["notice"], "search panel notice");
        if (notice !== null && (!notice.trim() || notice.length > 4096)) throw new ClientError("server", "Invalid search panel notice.");
        searchPanel = { kind: "selection", field, label, enableCounters: boolean(panel["enable_counters"], "search panel counters"), expand: boolean(panel["expand"], "search panel expand"), defaultValue, notice };
      } else throw new ClientError("unsupported", "Unknown search panel presentation.");
    }
    const pageLimit = type === "list" && rootNode["page_limit"] !== undefined ? integer(rootNode["page_limit"], "list page limit") : undefined;
    if (pageLimit !== undefined && pageLimit < 1) throw new ClientError("server", "List page limit must be a positive browser-safe integer.");
    const canCreate = (type === "list" || type === "form") && Object.hasOwn(rootNode, "create") ? boolean(rootNode["create"], "view create") : undefined;
    const canEdit = (type === "list" || type === "form") && Object.hasOwn(rootNode, "edit") ? boolean(rootNode["edit"], "view edit") : undefined;
    const canDelete = (type === "list" || type === "form") && Object.hasOwn(rootNode, "delete") ? boolean(rootNode["delete"], "view delete") : undefined;
    const tree = visit(root, 1);
    if (seen.size !== arena.length) throw new ClientError("server", "View contains unreachable nodes.");
    // Context references are host-authorized metadata, even when no field occurrence is rendered.
    // Read them into record state without making them editable or adding validation requirements.
    for (const name of contextFields) if (!selected.some(field => field.name === name)) {
      selected.push({...fields.get(name)!, readonly:true, required:false, invisible:true, forceSave:false});
    }
    views.set(type, {
      type,
      mode,
      nodes: [tree],
      fields: combineFields(selected),
      ...(searchPanel ? { searchPanel } : {}),
      ...(pageLimit === undefined ? {} : { pageLimit }),
      ...(canCreate === undefined ? {} : { canCreate }),
      ...(canEdit === undefined ? {} : { canEdit }),
      ...(canDelete === undefined ? {} : { canDelete }),
    });
  }
  if (!views.size) throw new ClientError("server", "No supported views were returned.");
  return { model, fields, views };
}
