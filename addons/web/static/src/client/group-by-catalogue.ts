/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { ClientError } from "./transport.js";
import type { Node, SearchFilterNode } from "./metadata.js";
import { GROUPABLE_KINDS, type GroupableField, type GroupableKind, type GroupByCatalogue } from "./search/types.js";

const MAX_FIELDS = 4096;
const FIELD_NAME = /^[a-z_][a-z0-9_]*$/;
const invalid = (what: string): never => { throw new ClientError("server", `Invalid ${what}.`); };
const isRecord = (value: unknown): value is Record<string, unknown> => typeof value === "object" && value !== null && !Array.isArray(value);

/** The fields the server reports as groupable (`fields_get` with `string`, `type`, `groupable`, `selection`),
 *  and for every other readable field why it is left out. */
export function decodeGroupableFields(raw: unknown): { groupable: Map<string, GroupableField>; excluded: Map<string, string> } {
  if (!isRecord(raw) || Object.keys(raw).length > MAX_FIELDS) return invalid("field list");
  const groupable = new Map<string, GroupableField>(), excluded = new Map<string, string>();
  for (const [name, value] of Object.entries(raw)) {
    if (!FIELD_NAME.test(name) || !isRecord(value)) return invalid("field description");
    const label = value["string"], type = value["type"], flag = value["groupable"];
    if (typeof label !== "string" || typeof type !== "string" || typeof flag !== "boolean") return invalid(`description of field ${name}`);
    if (!flag || name === "id") { excluded.set(name, "the server cannot group by it"); continue; }
    if (!(GROUPABLE_KINDS as readonly string[]).includes(type)) { excluded.set(name, `grouping by ${type} fields is not available yet`); continue; }
    let selection: GroupableField["selection"] = null;
    if (type === "selection") {
      const pairs = value["selection"];
      if (!Array.isArray(pairs) || pairs.length > 1024) return invalid(`selection of field ${name}`);
      selection = pairs.map(pair => {
        if (!Array.isArray(pair) || pair.length !== 2 || typeof pair[0] !== "string" || typeof pair[1] !== "string") return invalid(`selection of field ${name}`);
        return [pair[0], pair[1]] as const;
      });
    }
    groupable.set(name, { name, label, kind: type as GroupableKind, selection });
  }
  return { groupable, excluded };
}

/** Collect the search view's `<filter context="{'group_by': ...}">` items in view order. `evaluate` reads the
 *  filter's context expression; a filter whose context cannot be read or names a grouping this slice cannot
 *  show is returned in `skipped` with its reason, never dropped silently. A filter with another context is
 *  not a grouping item and is left to the filter menu. */
export function buildGroupByCatalogue(nodes: readonly Node[], evaluate: (context: NonNullable<SearchFilterNode["context"]>) => unknown, fields: ReturnType<typeof decodeGroupableFields>, intervals = false): GroupByCatalogue {
  const catalogue: GroupByCatalogue = { items: [], fields: [...fields.groupable.values()], skipped: [] };
  const names = new Set<string>();
  let position = 0;
  const visit = (node: Node): void => {
    if (node.kind === "search_filter" && node.context !== null) {
      const slot = position++, name = node.name || `#${slot}`;
      let context: unknown;
      try { context = evaluate(node.context); }
      catch (error) { catalogue.skipped.push({ description: node.label, reason: `its context cannot be read: ${error instanceof Error ? error.message : String(error)}` }); return; }
      if (!isRecord(context) || !Object.hasOwn(context, "group_by")) return;
      const spec = context["group_by"];
      const skip = (reason: string): void => { catalogue.skipped.push({ description: node.label, reason }); };
      if (Array.isArray(spec)) return skip("it groups by several fields, which is not available yet");
      if (typeof spec !== "string" || !spec) return skip("its group_by is not a field name");
      // The pivot view groups dates by an interval (`field:month`); the list does not yet.
      const [fieldName, interval, ...rest] = spec.split(":");
      if (interval !== undefined && (!intervals || rest.length || !["day", "week", "month", "quarter", "year"].includes(interval))) return skip("a date interval is not available yet");
      const field = fields.groupable.get(fieldName!);
      if (!field) return skip(fields.excluded.get(fieldName!) ?? `field ${fieldName} is not readable here`);
      if (names.has(name)) return skip(`another item is already named ${name}`);
      names.add(name);
      catalogue.items.push({ id: `groupby_${name}`, description: node.label, fieldName: field.name, ...(interval !== undefined ? { interval } : {}) });
    } else if ("children" in node) for (const child of node.children) visit(child);
  };
  for (const node of nodes) visit(node);
  return catalogue;
}
