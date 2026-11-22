/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { ClientError } from "../transport.js";
import type { Node, SearchFilterNode } from "../metadata.js";
import type { FilterItem } from "./types.js";
import type { SearchModel } from "./search-model.js";

/** A filter's `name` as it may appear in an address (`search_filters`) and in a `search_default_<name>` key. */
const FILTER_NAME = /^[A-Za-z_][A-Za-z0-9_]*$/;
/** The prefix of a declared filter's id: it keeps the view's names apart from the built-in and dashboard filters. */
export const VIEW_FILTER_PREFIX = "view_";
/** The first OR-group number of declared filters; the built-in Active/Archived and date filters use the numbers below. */
const FIRST_VIEW_GROUP = 1_000_000;
/** The prefix of a context key that preselects a search item (`search_default_<name>`). */
export const SEARCH_DEFAULT_PREFIX = "search_default_";
const MAX_DEFAULTS = 16;

const isRecord = (value: unknown): value is Record<string, unknown> => typeof value === "object" && value !== null && !Array.isArray(value);

/** What the search view declares for the Filters menu and for `search_default_<name>`. */
export interface SearchArch {
  /** The view's `<filter domain>` items in arch order. Consecutive filters share an OR group; a `<separator/>`,
   *  a `<field>` or a `<group>` boundary starts the next one, and groups are AND-ed. */
  filters: FilterItem[];
  /** Filters the menu leaves out, each with its reason. */
  skipped: { description: string; reason: string }[];
  /** `search_default_<name>` targets: a filter name and the id of its menu item. */
  filterIds: ReadonlyMap<string, string>;
  /** Names of the view's `<filter context="{'group_by': ...}">` items. */
  groupByNames: ReadonlySet<string>;
  /** Names of the view's `<field>` items. */
  fieldNames: ReadonlySet<string>;
}

/** Read the search view's named filters. `evaluate` reads a filter's domain or context expression (the same names
 *  as the group-by catalogue: `context` and `uid`); a filter whose expression cannot be read, or that sets a search
 *  context other than `group_by`, is returned in `skipped` with its reason, never dropped silently. */
export function readSearchArch(nodes: readonly Node[], evaluate: (ast: NonNullable<SearchFilterNode["domain"]>) => unknown): SearchArch {
  const filters: FilterItem[] = [], skipped: SearchArch["skipped"] = [];
  const filterIds = new Map<string, string>(), groupByNames = new Set<string>(), fieldNames = new Set<string>();
  let group = FIRST_VIEW_GROUP, open = false;
  const close = (): void => { if (open) { group++; open = false; } };
  const skip = (node: SearchFilterNode, reason: string): void => { skipped.push({ description: node.label, reason }); };
  const filter = (node: SearchFilterNode): void => {
    let context: unknown;
    if (node.context !== null) {
      try { context = evaluate(node.context); }
      catch (error) {
        // A filter without a domain is a group-by item: the Group By menu reports its unreadable context.
        if (node.domain !== null) skip(node, `its context cannot be read: ${error instanceof Error ? error.message : String(error)}`);
        return;
      }
      if (isRecord(context) && Object.hasOwn(context, "group_by")) { groupByNames.add(node.name); return; }
      if (!isRecord(context) || Object.keys(context).length) return skip(node, "it sets a search context, which is not available yet");
    }
    if (!FILTER_NAME.test(node.name)) return skip(node, `its name ${JSON.stringify(node.name.slice(0, 64))} is not a plain identifier`);
    if (filterIds.has(node.name)) return skip(node, `another filter is already named ${node.name}`);
    let domain: unknown = [];
    if (node.domain !== null) {
      try { domain = evaluate(node.domain); }
      catch (error) { return skip(node, `its domain cannot be read: ${error instanceof Error ? error.message : String(error)}`); }
      if (!Array.isArray(domain)) return skip(node, "its domain is not a list");
    }
    const id = `${VIEW_FILTER_PREFIX}${node.name}`;
    filterIds.set(node.name, id);
    filters.push({ id, description: node.label, domain: domain as unknown[], isActive: false, groupNumber: group });
    open = true;
  };
  const visit = (node: Node): void => {
    if (node.kind === "search_filter") filter(node);
    else if (node.kind === "search_separator") close();
    else if (node.kind === "search_field") { fieldNames.add(node.field.name); close(); }
    else if (node.kind === "search_group") { close(); node.children.forEach(visit); close(); }
    else if ("children" in node) node.children.forEach(visit);
  };
  nodes.forEach(visit);
  return { filters, skipped, filterIds, groupByNames, fieldNames };
}

/** Whether an address's `search_filters` names one of the search view's filters (malformed text is left to the search model). */
export function namesViewFilter(params: URLSearchParams): boolean {
  let ids: unknown;
  try { ids = JSON.parse(params.get("search_filters") ?? "[]"); } catch { return false; }
  return Array.isArray(ids) && ids.some(id => typeof id === "string" && id.startsWith(VIEW_FILTER_PREFIX));
}

/** Split a context into the keys that travel with requests and the `search_default_<name>` keys, which only
 *  choose the search the view opens with and never reach the server again. */
export function splitSearchDefaults(context: Readonly<Record<string, unknown>>): { context: Record<string, unknown>; defaults: Map<string, unknown> } {
  const rest: Record<string, unknown> = {}, defaults = new Map<string, unknown>();
  for (const [key, value] of Object.entries(context)) {
    if (!key.startsWith(SEARCH_DEFAULT_PREFIX)) { rest[key] = value; continue; }
    const name = key.slice(SEARCH_DEFAULT_PREFIX.length);
    if (!FILTER_NAME.test(name)) throw new ClientError("validation", `The context key ${JSON.stringify(key.slice(0, 80))} does not name a search item.`);
    if (!(typeof value === "string" || typeof value === "boolean" || typeof value === "number" && Number.isSafeInteger(value))) {
      throw new ClientError("validation", `The context key ${key} must be a text, a boolean or a whole number.`);
    }
    defaults.set(name, value);
  }
  if (defaults.size > MAX_DEFAULTS) throw new ClientError("validation", `A context may preselect at most ${MAX_DEFAULTS} search items.`);
  return { context: rest, defaults };
}

/** A default that was not applied, and why. */
export interface UnappliedDefault { name: string; reason: string }

/** Apply `search_default_<name>` values to a search that the address did not restore. A filter or group-by item is
 *  turned on by a truthy value; a field of the search view gets a facet with the value (a related record's id is shown
 *  by its name when `recordName` finds it). A falsy value leaves its item off. A name the view does not declare, or
 *  a value the item refuses, is returned with its reason: the caller tells the user, and the view still opens. */
export async function applySearchDefaults(model: SearchModel, arch: SearchArch, defaults: ReadonlyMap<string, unknown>,
  recordName?: (relation: string, id: number) => Promise<string>): Promise<UnappliedDefault[]> {
  const unapplied: UnappliedDefault[] = [];
  for (const [name, value] of defaults) {
    if (value === false || value === 0 || value === "") continue;
    try {
      const filterId = arch.filterIds.get(name);
      if (filterId !== undefined) {
        if (!model.getFilters().some(item => item.id === filterId && item.isActive)) model.toggleFilter(filterId);
      } else if (arch.groupByNames.has(name)) {
        if (!model.hasGroupBy()) { unapplied.push({ name, reason: "this view cannot be grouped" }); continue; }
        await model.loadGroupByCatalogue();
        const id = `groupby_${name}`;
        const item = model.getGroupBys().find(candidate => candidate.id === id);
        if (!item) { unapplied.push({ name, reason: model.getSkippedGroupBys().length ? "the Group By menu does not offer it here" : "it is not a group-by item here" }); continue; }
        if (!item.isActive) model.toggleGroupBy(id);
      } else if (arch.fieldNames.has(name)) {
        const field = model.fields.get(name);
        if (field?.kind === "many2one" && typeof value === "number") {
          const display = recordName && field.relation ? await recordName(field.relation, value) : undefined;
          model.addFieldFacet(name, String(value), "=", display);
        } else model.addFieldFacet(name, String(value));
      } else unapplied.push({ name, reason: "the search view declares no filter, group-by item or field with this name" });
    } catch (error) {
      if (!(error instanceof ClientError) || (error.kind !== "validation" && error.kind !== "unsupported")) throw error;
      unapplied.push({ name, reason: error.message });
    }
  }
  return unapplied;
}
