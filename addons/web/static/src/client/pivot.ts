/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The pivot view: any depth of row and column groupings, expanded one header at a
// time, measures, sort, flip, and the whole state kept in the page address. This file holds the pure parts (arch and
// field list to config, the model with its reads, the table it draws, the address encoding); `pivot-view.ts` draws it.
import { ClientError } from "./transport.js";
import { groupLabel } from "./list-groups.js";
import type { GroupableField, GroupableKind } from "./search/types.js";
import { GROUPABLE_KINDS } from "./search/types.js";
import type { Field, PivotFieldNode, View } from "./metadata.js";

export const PIVOT_INTERVALS = ["day", "week", "month", "quarter", "year"] as const;
export type PivotInterval = (typeof PIVOT_INTERVALS)[number];
export type PivotKind = GroupableKind | "date" | "datetime";
const PIVOT_KINDS: readonly string[] = [...GROUPABLE_KINDS, "date", "datetime"];
/** Groupings per axis: a read crosses every depth of one axis with every depth of the other, and one call takes at most 64 sets. */
export const PIVOT_MAX_LEVELS = 6;
/** Measures shown at once (the address and the call carry at most this many). */
export const PIVOT_MAX_MEASURES = 16;
const MAX_DOMAIN_TOKENS = 256;
const MAX_LABEL = 4096;
const invalid = (what: string): never => { throw new ClientError("server", `Invalid ${what}.`); };
const refuse = (message: string): never => { throw new ClientError("validation", message); };
const unsupported = (message: string): never => { throw new ClientError("unsupported", message); };

/** One grouping: a field and, for dates, the interval (month when none is declared). */
export interface PivotField extends GroupableField<PivotKind> { interval: PivotInterval | null }
export interface PivotMeasure { id: string; label: string; /** What `aggregates` carries: `__count` or `field:sum`. */ spec: string; kind: "integer" | "float" }
export interface PivotSort { measure: string; /** The column (its group keys) the rows are ordered by; empty for the Total column. */ path: string[]; desc: boolean }
export interface PivotLimits {
  /** The most groups one grouping may return (setting `web.pivot_max_groups`). */
  maxGroups: number;
  /** The most headers that may stay expanded (setting `web.pivot_max_open_headers`). */
  maxOpen: number;
  /** The most cells the table may hold (setting `web.pivot_max_cells`). */
  maxCells: number;
}
export interface PivotGroupItem { id: string; description: string; field: PivotField }
export interface PivotConfig {
  rows: readonly PivotField[];
  cols: readonly PivotField[];
  /** Every measure the menu offers: Count, then the aggregatable fields by label. */
  measures: readonly PivotMeasure[];
  /** The measures that start active (Count when the arch names none, or `display_quantity`). */
  initial: readonly string[];
  disableLinking: boolean;
  defaultOrder: { measure: string; desc: boolean } | null;
  /** The fields a header can be expanded by, by name. */
  groupable: ReadonlyMap<string, PivotField>;
  /** The search view's group-by items, offered first in the expand menu. */
  items: readonly PivotGroupItem[];
}

export const COUNT: PivotMeasure = { id: "__count", label: "Count", spec: "__count", kind: "integer" };
const isDate = (field: { kind: string }): boolean => field.kind === "date" || field.kind === "datetime";
export const specOf = (field: PivotField): string => field.interval ? `${field.name}:${field.interval}` : field.name;
const isRecord = (value: unknown): value is Record<string, unknown> => typeof value === "object" && value !== null && !Array.isArray(value);

/** A grouping from its `field` or `field:interval` text. A date with no interval groups by month. */
export function resolveSpec(spec: unknown, groupable: ReadonlyMap<string, PivotField>): PivotField {
  if (typeof spec !== "string") return refuse("A pivot grouping must be a field name.");
  const [name, interval, ...rest] = spec.split(":");
  const base = groupable.get(name!);
  if (!base || rest.length) return refuse(`The pivot cannot group by ${spec}.`);
  if (!isDate(base)) return interval === undefined ? { ...base, interval: null } : refuse(`${base.label} is not a date field, so ${spec} has no interval.`);
  if (interval !== undefined && !(PIVOT_INTERVALS as readonly string[]).includes(interval)) return refuse(`${spec} names an interval that is not one of ${PIVOT_INTERVALS.join(", ")}.`);
  return { ...base, interval: (interval ?? "month") as PivotInterval };
}

/** What `fields_get` (attributes string, type, groupable, selection, aggregator) says: the fields a header can be expanded by and the fields
 *  that can be summed. A field the client cannot group by is left out, never guessed. */
export function decodePivotFields(raw: unknown): { groupable: Map<string, PivotField>; measures: PivotMeasure[] } {
  if (!isRecord(raw) || Object.keys(raw).length > 4096) return invalid("field list");
  const groupable = new Map<string, PivotField>(), measures: PivotMeasure[] = [];
  for (const [name, value] of Object.entries(raw)) {
    if (!/^[a-z_][a-z0-9_]*$/.test(name) || !isRecord(value)) return invalid("field description");
    const { string: label, type, groupable: flag, aggregator } = value as Record<string, unknown>;
    if (typeof label !== "string" || typeof type !== "string" || typeof flag !== "boolean") return invalid(`description of field ${name}`);
    if (flag && name !== "id" && PIVOT_KINDS.includes(type)) {
      let selection: PivotField["selection"] = null;
      if (type === "selection") {
        const pairs = value["selection"];
        if (!Array.isArray(pairs) || pairs.length > 1024) return invalid(`selection of field ${name}`);
        selection = pairs.map(pair => Array.isArray(pair) && pair.length === 2 && typeof pair[0] === "string" && typeof pair[1] === "string" ? [pair[0], pair[1]] as const : invalid(`selection of field ${name}`));
      }
      groupable.set(name, { name, label, kind: type as PivotKind, selection, interval: null });
    }
    if (aggregator === "sum" && (type === "integer" || type === "float" || type === "monetary") && name !== "id") {
      measures.push({ id: name, label, spec: `${name}:sum`, kind: type === "integer" ? "integer" : "float" });
    }
  }
  measures.sort((a, b) => a.label.localeCompare(b.label));
  return { groupable, measures };
}

/** The pivot arch as the server delivered it (`pivot` root, `pivot_field` children) and the model's field list, as the table needs them. */
export function readPivotView(view: View, catalogue: { groupable: ReadonlyMap<string, PivotField>; measures: readonly PivotMeasure[]; items?: readonly PivotGroupItem[] }): PivotConfig {
  const root = view.nodes[0];
  if (view.type !== "pivot" || !root || root.kind !== "pivot") return invalid("pivot view");
  const axes: Record<"row" | "col", PivotFieldNode[]> = { row: [], col: [] }, declared: PivotFieldNode[] = [];
  for (const child of root.children) {
    if (child.kind !== "pivot_field") return invalid("pivot child");
    if (child.role === "measure") declared.push(child); else axes[child.role].push(child);
  }
  const grouping = (node: PivotFieldNode): PivotField => {
    const { field } = node;
    if (!PIVOT_KINDS.includes(field.kind)) return unsupported(`Grouping by ${node.label} (a ${field.kind} field) is not available in the pivot view.`);
    const interval = isDate(field) ? (node.interval ?? "month") as PivotInterval : null;
    return { name: field.name, label: node.label, kind: field.kind as PivotKind, selection: field.selection, interval };
  };
  const measureOf = (node: PivotFieldNode): PivotMeasure => {
    const kind: Field["kind"] = node.field.kind;
    if (kind !== "integer" && kind !== "float" && kind !== "monetary") return unsupported(`The measure ${node.label} (a ${kind} field) is not available in the pivot view.`);
    return { id: node.field.name, label: node.label, spec: `${node.field.name}:sum`, kind: kind === "integer" ? "integer" : "float" };
  };
  const named = declared.map(measureOf);
  if (new Set(named.map(measure => measure.id)).size !== named.length) return invalid("pivot measures: a field is declared twice");
  const byId = new Map<string, PivotMeasure>(catalogue.measures.map(measure => [measure.id, measure]));
  for (const measure of named) byId.set(measure.id, measure); // the arch's label wins
  const initial = named.map(measure => measure.id);
  if (!initial.length || root.displayQuantity) initial.unshift(COUNT.id);
  let defaultOrder: PivotConfig["defaultOrder"] = null;
  if (root.defaultOrder !== null) {
    const [measure, direction] = root.defaultOrder.split(" ");
    // The server checked the measure when the view was declared; one the caller may not read was pruned, and then there is nothing to sort by.
    if (measure === COUNT.id || byId.has(measure!)) defaultOrder = { measure: measure!, desc: direction === "desc" };
  }
  return {
    rows: axes.row.map(grouping), cols: axes.col.map(grouping),
    measures: [COUNT, ...[...byId.values()].sort((a, b) => a.label.localeCompare(b.label))], initial,
    disableLinking: root.disableLinking, defaultOrder, groupable: catalogue.groupable, items: catalogue.items ?? []
  };
}

/** The context keys `pivot_measures`, `pivot_row_groupby` and `pivot_column_groupby` replace what the arch declares (reference `pivot_model.js` `load`). */
export function applyContext(config: PivotConfig, context: Record<string, unknown>): PivotConfig {
  const list = (key: string, max: number): string[] | undefined => {
    if (!Object.hasOwn(context, key)) return undefined;
    const value = context[key];
    if (!Array.isArray(value) || value.length > max || value.some(item => typeof item !== "string")) return refuse(`The context key ${key} must be a list of at most ${max} names.`);
    return value as string[];
  };
  const rows = list("pivot_row_groupby", PIVOT_MAX_LEVELS), cols = list("pivot_column_groupby", PIVOT_MAX_LEVELS), measures = list("pivot_measures", PIVOT_MAX_MEASURES);
  if (!rows && !cols && !measures) return config;
  const known = new Set(config.measures.map(measure => measure.id));
  for (const id of measures ?? []) if (!known.has(id)) refuse(`The context key pivot_measures names ${id}, which is not a measure of this model.`);
  return {
    ...config,
    rows: rows ? rows.map(spec => resolveSpec(spec, config.groupable)) : config.rows,
    cols: cols ? cols.map(spec => resolveSpec(spec, config.groupable)) : config.cols,
    initial: measures ?? config.initial
  };
}

// ---- the model ----

type AxisName = "row" | "col";
const axisIndex = (axis: AxisName): 0 | 1 => axis === "row" ? 0 : 1;
/** Group keys from the root down; each key is the JSON of the group value. */
type Path = readonly string[];
const pk = (path: Path): string => path.join("\u0000");
const cellKey = (row: Path, col: Path): string => `${pk(row)}\u0001${pk(col)}`;

interface Axis { levels: PivotField[]; open: Map<string, string[]>; /** Every level is expanded; filled in from the data on the next read. */ all: boolean; /** Open is "everything within the levels" (what a pristine arch shows). */ full: boolean }
interface State { axes: [Axis, Axis]; active: string[]; sort: PivotSort | null }
interface Header { key: string; label: string; domain: unknown[] }
interface Cell { values: Map<string, number | null> }
interface Part {
  heads: [Map<string, Header[]>, Map<string, Header[]>];
  domains: [Map<string, unknown[]>, Map<string, unknown[]>];
  cells: Map<string, Cell>;
  covered: string[];
}
interface Held extends Omit<Part, "covered"> { covered: Set<string>; aggregates: string[] }
/** The grouping sets of one read: `r` row levels crossed with `c` column levels. */
interface SetPlan { r: number; c: number }
interface Restriction { axis: 0 | 1; path: string[] }

const emptyPart = (): Part => ({ heads: [new Map(), new Map()], domains: [new Map(), new Map()], cells: new Map(), covered: [] });
const emptyHeld = (): Held => ({ ...emptyPart(), covered: new Set(), aggregates: [] });
const cloneAxis = (axis: Axis): Axis => ({ ...axis, levels: [...axis.levels], open: new Map(axis.open) });
const cloneState = (state: State): State => ({ axes: [cloneAxis(state.axes[0]), cloneAxis(state.axes[1])], active: [...state.active], sort: state.sort && { ...state.sort, path: [...state.sort.path] } });
/** How many grouping levels the headers on screen need. */
const used = (axis: Axis): number => axis.all ? axis.levels.length : axis.open.size ? 1 + Math.max(...[...axis.open.values()].map(path => path.length)) : 0;
const covKey = (axes: [Axis, Axis], plan: SetPlan, res: Restriction | null): string =>
  `${axes[0].levels.slice(0, plan.r).map(specOf).join(",")}\u0002${axes[1].levels.slice(0, plan.c).map(specOf).join(",")}\u0002${res ? `${res.axis}:${pk(res.path)}` : ""}`;

const keyOf = (field: PivotField, value: unknown): string => {
  if ((field.kind === "many2one" || isDate(field)) && Array.isArray(value)) return JSON.stringify(value[0]);
  return JSON.stringify(value);
};
const labelOf = (field: PivotField, value: unknown): string => {
  if (isDate(field)) {
    if (value === false) return "None";
    if (!Array.isArray(value) || value.length !== 2 || typeof value[0] !== "string" || typeof value[1] !== "string") return invalid(`group value of ${field.name}`);
    return value[1];
  }
  return groupLabel(field as unknown as GroupableField, value);
};

/** Validate the answer of `formatted_read_grouping_sets`: one list of rows per requested set, closed keys, checked group values,
 *  counts and aggregates; the groups become headers (pure axis sets) and cells (every set). */
export function decodePivotAnswer(raw: unknown, plans: readonly SetPlan[], axes: readonly [readonly PivotField[], readonly PivotField[]], aggregates: readonly string[], limits: PivotLimits): Part {
  if (!Array.isArray(raw) || raw.length !== plans.length) return invalid("pivot answer");
  const part = emptyPart();
  plans.forEach((plan, index) => {
    const rows = raw[index];
    if (!Array.isArray(rows)) return invalid("pivot answer");
    const fields = [...axes[0].slice(0, plan.r), ...axes[1].slice(0, plan.c)];
    // Groups per level is the bound of a pure axis set; a crossed set may hold up to the table's cell bound.
    const bound = plan.r === 0 || plan.c === 0 ? limits.maxGroups : limits.maxCells;
    if (rows.length > bound) {
      const names = fields.map(field => field.label).join(" and ");
      return refuse(plan.r === 0 || plan.c === 0
        ? `Grouping by ${names || "the Total"} returns more than ${limits.maxGroups} groups, the most the pivot view shows (set in Settings: Pivot view, most groups). Pick another field or narrow the search.`
        : `Grouping by ${names} gives more than ${limits.maxCells} cells, the most the pivot view holds (set in Settings: Pivot view, most cells). Pick another field or narrow the search.`);
    }
    const allowed = new Set<string>([...fields.map(specOf), "__extra_domain", ...aggregates]);
    part.covered.push(`${plan.r}:${plan.c}`);
    const seen = new Set<string>();
    for (const item of rows) {
      if (!isRecord(item)) return invalid("pivot group");
      if (Object.keys(item).some(key => !allowed.has(key))) return invalid("pivot group property");
      const domain = item["__extra_domain"];
      if (!Array.isArray(domain) || domain.length > MAX_DOMAIN_TOKENS) return invalid("pivot group domain");
      const keys: string[] = [], labels: string[] = [];
      for (const field of fields) {
        const spec = specOf(field);
        if (!Object.hasOwn(item, spec)) return invalid(`pivot group value of ${spec}`);
        const label = labelOf(field, item[spec]);
        if (label.length > MAX_LABEL) return invalid("pivot group label");
        keys.push(keyOf(field, item[spec])); labels.push(label);
      }
      const values = new Map<string, number | null>();
      for (const spec of aggregates) {
        const value = item[spec];
        if (spec === "__count" && !(typeof value === "number" && Number.isSafeInteger(value) && value >= 0)) return invalid("pivot group count");
        if (value === null || value === false) values.set(spec, null);
        else if (typeof value === "number" && Number.isFinite(value)) values.set(spec, value);
        else return invalid(`pivot value of ${spec}`);
      }
      const whole = keys.join("\u0000");
      if (seen.has(whole)) return invalid("pivot group list: duplicate group");
      seen.add(whole);
      const rowPath = keys.slice(0, plan.r), colPath = keys.slice(plan.r);
      part.cells.set(cellKey(rowPath, colPath), { values });
      if (plan.c === 0 && plan.r > 0) {
        const parent = pk(rowPath.slice(0, -1)), list = part.heads[0].get(parent) ?? [];
        list.push({ key: rowPath[rowPath.length - 1]!, label: labels[plan.r - 1]!, domain }); part.heads[0].set(parent, list);
        part.domains[0].set(pk(rowPath), domain);
      } else if (plan.r === 0 && plan.c > 0) {
        const parent = pk(colPath.slice(0, -1)), list = part.heads[1].get(parent) ?? [];
        list.push({ key: colPath[colPath.length - 1]!, label: labels[plan.c - 1]!, domain }); part.heads[1].set(parent, list);
        part.domains[1].set(pk(colPath), domain);
      } else if (plan.r === 0 && plan.c === 0) { part.domains[0].set("", domain); part.domains[1].set("", domain); }
    }
    return undefined;
  });
  return part;
}

/** `domain` is the search the table on screen was built from (captured when its base read started), narrowed by a header's own domain
 *  for a partial read; never the search at the moment of the call. */
export type PivotCall = (request: { groupingSets: string[][]; aggregates: string[]; domain: unknown[] }) => Promise<unknown>;

export interface RowHeader { path: string[]; label: string; depth: number; open: boolean }
export interface ColHeader { path: string[]; label: string; depth: number; open: boolean; /** Leaf columns under it. */ span: number; rowSpan: number }
export interface PivotTable {
  /** The Total row first, then every visible row in order. */
  rows: RowHeader[];
  /** One list per header depth, left to right; the first holds the Total header. */
  headerRows: ColHeader[][];
  /** The leaf columns, left to right (the Total's own path when the column axis is folded). */
  leaves: string[][];
  /** The Total column is its own column only when more than one leaf exists. */
  totalColumn: boolean;
  measures: PivotMeasure[];
  hasData: boolean;
  /** The last read failed: the held figures belong to an older search and are not shown. */
  unavailable: boolean;
  sort: PivotSort | null;
  /** `undefined`: no records in that cell; `null`: records, but the aggregate has no value. */
  cell(row: Path, col: Path, measure: PivotMeasure): number | null | undefined;
}

/** What the address keeps (values are the raw group values, not their JSON). */
export interface PivotSnapshot {
  rows: string[]; cols: string[];
  open: { row: unknown[][]; col: unknown[][] };
  measures: string[];
  sort: { measure: string; desc: boolean; path: unknown[] } | null;
  /** The layout (groupings and expanded headers), the measures and the sort are what the arch declares. */
  layoutDefault: boolean; measuresDefault: boolean; sortDefault: boolean;
}
/** The state a model starts from: the arch's, or the address's. `open: null` expands every level. */
export interface PivotInitial { rows: readonly PivotField[]; cols: readonly PivotField[]; open: { row: string[][]; col: string[][] } | null; active: readonly string[]; sort: PivotSort | null }

export function archState(config: PivotConfig): PivotInitial {
  return { rows: config.rows, cols: config.cols, open: null, active: config.initial, sort: config.defaultOrder && { measure: config.defaultOrder.measure, path: [], desc: config.defaultOrder.desc } };
}

export class PivotModel {
  private state: State;
  private held: Held = emptyHeld();
  /** The search the held figures were read under: every partial read merged into them uses it. */
  private heldDomain: unknown[] = [];
  /** Model jobs run one after the other, so an answer can never land on a state a later job already moved on from. */
  private tail: Promise<unknown> = Promise.resolve();
  private stale = false;
  private dead = false;
  private readonly defaults: { rows: string; cols: string; measures: string; sort: string };

  /** `currentDomain` is the search as it is now; the model reads it only when a base read starts. `accept` may refuse a state before anything is read
   *  (the address cannot hold it). */
  constructor(readonly config: PivotConfig, private readonly call: PivotCall, private readonly currentDomain: () => unknown[], private readonly limits: PivotLimits, initial: PivotInitial = archState(config), private readonly accept: (snapshot: PivotSnapshot) => void = () => undefined) {
    if (initial.rows.length > PIVOT_MAX_LEVELS || initial.cols.length > PIVOT_MAX_LEVELS) refuse(`The pivot groups by at most ${PIVOT_MAX_LEVELS} fields on an axis.`);
    if (initial.active.length > PIVOT_MAX_MEASURES) refuse(`The pivot shows at most ${PIVOT_MAX_MEASURES} measures.`);
    for (const id of initial.active) if (!config.measures.some(measure => measure.id === id)) refuse(`Unknown pivot measure ${id}.`);
    const axis = (levels: readonly PivotField[], open: string[][] | undefined): Axis => {
      const map = new Map<string, string[]>();
      for (const path of open ?? []) {
        if (path.length >= levels.length) refuse("The address expands a header deeper than its groupings.");
        map.set(pk(path), path);
      }
      return { levels: [...levels], open: map, all: open === undefined, full: open === undefined };
    };
    this.state = { axes: [axis(initial.rows, initial.open?.row), axis(initial.cols, initial.open?.col)], active: [...initial.active], sort: initial.sort };
    this.defaults = { rows: config.rows.map(specOf).join(","), cols: config.cols.map(specOf).join(","), measures: config.initial.join(","), sort: JSON.stringify(archState(config).sort) };
  }

  get active(): readonly string[] { return this.state.active; }
  get sort(): PivotSort | null { return this.state.sort; }
  levelAt(axis: AxisName, depth: number): PivotField | undefined { return this.state.axes[axisIndex(axis)].levels[depth]; }
  get levelCount(): { row: number; col: number } { return { row: this.state.axes[0].levels.length, col: this.state.axes[1].levels.length }; }

  /** After `dispose` a job that was waiting or in flight changes nothing. */
  dispose(): void { this.dead = true; }

  private enqueue(job: () => Promise<void>): Promise<void> {
    const run = this.tail.then(() => this.dead ? undefined : job());
    this.tail = run.catch(() => undefined);
    return run;
  }

  private aggregatesOf(active: readonly string[], sort: PivotSort | null): string[] {
    // Count is always read: it is how a table with no records is told from one whose aggregates are empty. The sort's measure stays read when hidden.
    const ids = sort ? [...active, sort.measure] : active;
    return [...new Set(["__count", ...ids.flatMap(id => this.config.measures.find(measure => measure.id === id)?.spec ?? [])])];
  }

  /** Read the whole table again (first load, a new search). If the read fails the held figures (read under the old search) are marked
   *  unavailable: the next job reads everything again instead of mixing domains. */
  load(): Promise<void> { return this.enqueue(() => this.reload(this.state, true)); }

  private snapshotOf(state: State): PivotSnapshot {
    const raw = (path: readonly string[]): unknown[] => path.map(key => JSON.parse(key) as unknown);
    const [rows, cols] = state.axes;
    return {
      rows: rows.levels.map(specOf), cols: cols.levels.map(specOf),
      open: { row: [...rows.open.values()].map(raw), col: [...cols.open.values()].map(raw) },
      measures: [...state.active],
      sort: state.sort && { measure: state.sort.measure, desc: state.sort.desc, path: raw(state.sort.path) },
      layoutDefault: rows.full && cols.full && rows.levels.map(specOf).join(",") === this.defaults.rows && cols.levels.map(specOf).join(",") === this.defaults.cols,
      measuresDefault: state.active.join(",") === this.defaults.measures,
      sortDefault: JSON.stringify(state.sort) === this.defaults.sort
    };
  }
  snapshot(): PivotSnapshot { return this.snapshotOf(this.state); }

  private checkOpen(state: State): void {
    const open = state.axes[0].open.size + state.axes[1].open.size;
    if (open > this.limits.maxOpen) refuse(`More than ${this.limits.maxOpen} headers would stay expanded, the most the pivot view allows (set in Settings: Pivot view, most headers that may stay expanded). Collapse some headers first.`);
  }

  private plans(rowsUsed: number, colsUsed: number): SetPlan[] {
    const plans: SetPlan[] = [];
    for (let r = 0; r <= rowsUsed; r++) for (let c = 0; c <= colsUsed; c++) plans.push({ r, c });
    return plans;
  }

  private async read(plans: readonly SetPlan[], axes: [Axis, Axis], aggregates: string[], domain: unknown[]): Promise<Part> {
    const levels: [readonly PivotField[], readonly PivotField[]] = [axes[0].levels, axes[1].levels];
    const groupingSets = plans.map(plan => [...levels[0].slice(0, plan.r), ...levels[1].slice(0, plan.c)].map(specOf));
    const answer = await this.call({ groupingSets, aggregates, domain });
    return decodePivotAnswer(answer, plans, levels, aggregates, this.limits);
  }

  private merge(held: Held, part: Part, axes: [Axis, Axis], res: Restriction | null): void {
    for (const axis of [0, 1] as const) {
      for (const [parent, list] of part.heads[axis]) held.heads[axis].set(parent, list);
      for (const [path, domain] of part.domains[axis]) held.domains[axis].set(path, domain);
    }
    for (const [key, cell] of part.cells) held.cells.set(key, cell);
    for (const plan of part.covered) { const [r, c] = plan.split(":").map(Number); held.covered.add(covKey(axes, { r: r!, c: c! }, res)); }
  }

  /** Expanding every level fills the open set from the data; the open set is also cut to the headers that exist. */
  private settleOpen(axis: Axis, index: 0 | 1, held: Held): void {
    const open = new Map<string, string[]>();
    const walk = (path: string[]): void => {
      const here = pk(path);
      if (path.length >= axis.levels.length || !(axis.all || axis.open.has(here))) return;
      open.set(here, path);
      if (open.size > this.limits.maxOpen) refuse(`The table would have more than ${this.limits.maxOpen} expanded headers, the most the pivot view allows (set in Settings: Pivot view, most headers that may stay expanded). Group by fewer fields or narrow the search.`);
      for (const child of held.heads[index].get(here) ?? []) walk([...path, child.key]);
    };
    walk([]);
    axis.open = open; axis.all = false;
  }

  private async reload(next: State, fresh = false): Promise<void> {
    const state = cloneState(next);
    const aggregates = this.aggregatesOf(state.active, state.sort);
    const domain = this.currentDomain(); // captured when the base read starts, as the list does
    const plans = this.plans(used(state.axes[0]), used(state.axes[1]));
    let part: Part;
    try { part = await this.read(plans, state.axes, aggregates, domain); }
    catch (error) { if (!this.dead) this.stale = true; throw error; }
    if (this.dead) return;
    try {
      const held = emptyHeld();
      this.merge(held, part, state.axes, null);
      held.aggregates = aggregates;
      this.settleOpen(state.axes[0], 0, held);
      this.settleOpen(state.axes[1], 1, held);
      this.checkOpen(state);
      if (held.cells.size > this.limits.maxCells) refuse(`The table would have more than ${this.limits.maxCells} cells, the most the pivot view holds (set in Settings: Pivot view, most cells). Group by fewer fields or narrow the search.`);
      this.accept(this.snapshotOf(state)); // Expand all fills the open set from the data: it must still fit the address
      this.held = held; this.heldDomain = domain; this.state = state; this.stale = false;
    } catch (error) {
      // A new search refused after its read must not leave the old search's figures on screen (also when it arrives as a refused Group By); a refused change under the same search keeps its table.
      if (fresh || JSON.stringify(domain) !== JSON.stringify(this.heldDomain)) this.stale = true;
      throw error;
    }
  }

  /** The reads a state still needs, one per header whose children are missing (a header's partial read is narrowed to that header). */
  private missing(state: State): { res: Restriction | null; plans: Map<string, SetPlan> }[] {
    const groups = new Map<string, { res: Restriction | null; plans: Map<string, SetPlan> }>();
    const need = (res: Restriction | null, plan: SetPlan): void => {
      if (this.held.covered.has(covKey(state.axes, plan, res)) || this.held.covered.has(covKey(state.axes, plan, null))) return;
      const id = res ? `${res.axis}:${pk(res.path)}` : "";
      const group = groups.get(id) ?? { res, plans: new Map() };
      group.plans.set(`${plan.r}:${plan.c}`, plan); groups.set(id, group);
    };
    const [rows, cols] = state.axes;
    need(null, { r: 0, c: 0 });
    // Only the headers this change opens need data: the children of a new row header against every column depth on screen, and the
    // children of a new column header against every row depth. A cell between two new headers is read by whichever side asks first.
    const opened = (axis: 0 | 1): string[][] => [...state.axes[axis].open].filter(([key]) => !this.state.axes[axis].open.has(key)).map(([, path]) => path);
    for (const path of opened(0)) for (let c = 0; c <= used(cols); c++) need(path.length ? { axis: 0, path } : null, { r: path.length + 1, c });
    for (const path of opened(1)) for (let r = 0; r <= used(rows); r++) need(path.length ? { axis: 1, path } : null, { r, c: path.length + 1 });
    return [...groups.values()];
  }

  private async transition(next: State, options: { full?: boolean } = {}): Promise<void> {
    this.checkOpen(next);
    this.accept(this.snapshotOf(next));
    const spec = (id: string): string => this.config.measures.find(measure => measure.id === id)!.spec;
    if (options.full || this.stale || next.active.some(id => !this.held.aggregates.includes(spec(id)))) return this.reload(next);
    const fetched: { part: Part; res: Restriction | null }[] = [];
    for (const group of this.missing(next)) {
      let domain = this.heldDomain;
      if (group.res) {
        const own = this.held.domains[group.res.axis].get(pk(group.res.path));
        if (!own) return invalid("pivot header: its records are not known");
        domain = [...this.heldDomain, ...own];
      }
      fetched.push({ part: await this.read([...group.plans.values()], next.axes, this.held.aggregates, domain), res: group.res });
      if (this.dead) return;
    }
    let added = 0;
    for (const { part } of fetched) for (const key of part.cells.keys()) if (!this.held.cells.has(key)) added++;
    if (this.held.cells.size + added > this.limits.maxCells) refuse(`The table would have more than ${this.limits.maxCells} cells, the most the pivot view holds (set in Settings: Pivot view, most cells). Collapse some headers or narrow the search.`);
    for (const { part, res } of fetched) this.merge(this.held, part, next.axes, res);
    this.state = next;
  }

  /** Apply an edit of the state and make it current: the reads it needs, then the commit. Runs on the queue (through `change`). */
  private async applyNow(edit: (state: State) => void, options?: { full?: boolean }): Promise<void> {
    const next = cloneState(this.state);
    edit(next);
    const before = this.state.axes.map(axis => axis.levels.length);
    await this.transition(next, options);
    if (this.dead || this.state !== next) return;
    // Groupings that no header uses any more are dropped: what was read under them must not answer a later grouping that reuses a key.
    next.axes.forEach((axis, index) => { if (axis.levels.length < before[index]!) this.purge(index as 0 | 1, axis.levels.length); });
  }

  private change(edit: (state: State) => void, options?: { full?: boolean }): Promise<void> {
    return this.enqueue(() => this.applyNow(edit, options));
  }

  private purge(axis: 0 | 1, levels: number): void {
    const length = (path: string): number => path === "" ? 0 : path.split("\u0000").length;
    for (const key of [...this.held.cells.keys()]) if (length(key.split("\u0001")[axis]!) > levels) this.held.cells.delete(key);
    for (const parent of [...this.held.heads[axis].keys()]) if (length(parent) >= levels) this.held.heads[axis].delete(parent);
    for (const path of [...this.held.domains[axis].keys()]) if (length(path) > levels) this.held.domains[axis].delete(path);
    for (const key of [...this.held.covered]) {
      const specs = key.split("\u0002")[axis]!;
      if ((specs === "" ? 0 : specs.split(",").length) > levels) this.held.covered.delete(key);
    }
  }

  private expandEdit(axis: AxisName, path: Path, field?: PivotField): (state: State) => void {
    return state => {
      const target = state.axes[axisIndex(axis)], depth = path.length;
      if (target.open.has(pk(path))) return;
      if (field && target.levels[depth] && specOf(target.levels[depth]!) !== specOf(field)) refuse(`This header is already grouped by ${target.levels[depth]!.label}; collapse the headers at that depth to choose another field.`);
      if (!target.levels[depth]) {
        if (!field) return refuse("Choose a field to expand this header by.");
        if (depth !== target.levels.length) return refuse("A header can only be expanded one level deeper than the groupings so far.");
        if (depth >= PIVOT_MAX_LEVELS) return refuse(`The pivot groups by at most ${PIVOT_MAX_LEVELS} fields on an axis.`);
        target.levels.push(field);
      }
      target.open.set(pk(path), [...path]); target.full = false;
    };
  }

  private collapseEdit(axis: AxisName, path: Path): (state: State) => void {
    return state => {
      const target = state.axes[axisIndex(axis)];
      if (!target.open.has(pk(path))) return;
      for (const [key, open] of target.open) if (open.length >= path.length && path.every((item, i) => open[i] === item)) target.open.delete(key);
      target.levels.length = Math.min(target.levels.length, used(target));
      target.full = false;
      // A sort under a column that is folded away names a column that no longer exists.
      if (axis === "col" && state.sort && state.sort.path.length > path.length && path.every((item, i) => state.sort!.path[i] === item)) state.sort = null;
    };
  }

  /** Expand the header at `path` by `field`, or by the next grouping already declared when none is given. One grouped read, narrowed to that header. */
  expand(axis: AxisName, path: Path, field?: PivotField): Promise<void> { return this.change(this.expandEdit(axis, path, field)); }
  /** Collapse the header at `path` and everything below it. Groupings no header uses any more are dropped, so the next expansion chooses again. */
  collapse(axis: AxisName, path: Path): Promise<void> { return this.change(this.collapseEdit(axis, path)); }

  /** Fold or unfold to a stated state (the arrow keys): asking for the state a queued job already reaches changes nothing. */
  setOpen(axis: AxisName, path: Path, open: boolean, field?: PivotField): Promise<void> {
    return this.enqueue(async () => {
      if (this.state.axes[axisIndex(axis)].open.has(pk(path)) === open) return;
      await this.applyNow(open ? this.expandEdit(axis, path, field) : this.collapseEdit(axis, path));
    });
  }

  /** The search's Group By sets the row groupings (reference `pivot_model.js`): in the order given, every level expanded, what header clicks added is
   *  forgotten. `null` returns to the arch's rows. The columns are not touched. Reads the whole table again. */
  setRows(fields: readonly PivotField[] | null): Promise<void> {
    const levels = fields ?? this.config.rows;
    if (levels.length > PIVOT_MAX_LEVELS) return Promise.reject(new ClientError("validation", `The pivot groups by at most ${PIVOT_MAX_LEVELS} fields on an axis.`));
    return this.change(state => { state.axes[0] = { levels: [...levels], open: new Map(), all: true, full: true }; }, { full: true });
  }

  expandAll(): Promise<void> { return this.change(state => { for (const axis of state.axes) { axis.all = axis.levels.length > 0; axis.open = new Map(); axis.full = true; } }, { full: true }); }
  /** Folds both Totals; the groupings stay, so the next click on a Total expands by them again and Expand all restores the table. */
  collapseAll(): Promise<void> { return this.change(state => { for (const axis of state.axes) { axis.open = new Map(); axis.all = false; axis.full = false; } if (state.sort?.path.length) state.sort = null; /* its column is folded away */ }); }

  /** Swap rows and columns. Reads nothing: the held cells are re-keyed. */
  flip(): Promise<void> {
    return this.enqueue(async () => {
      const next = cloneState(this.state);
      next.axes = [next.axes[1], next.axes[0]]; next.sort = null;
      this.accept(this.snapshotOf(next));
      const held = this.held;
      held.heads = [held.heads[1], held.heads[0]]; held.domains = [held.domains[1], held.domains[0]];
      held.cells = new Map([...held.cells].map(([key, cell]) => { const [row, col] = key.split("\u0001"); return [`${col}\u0001${row}`, cell] as const; }));
      held.covered = new Set([...held.covered].map(key => { const [r, c, res] = key.split("\u0002"); return `${c}\u0002${r}\u0002${res ? `${res.startsWith("0") ? 1 : 0}${res.slice(1)}` : ""}`; }));
      this.state = next;
    });
  }

  /** Turn a measure off (local, no read) or on (one read with the new aggregates). */
  toggleMeasure(id: string): Promise<void> {
    if (!this.config.measures.some(measure => measure.id === id)) return Promise.reject(new ClientError("validation", `Unknown pivot measure ${id}.`));
    return this.change(state => {
      if (state.active.includes(id)) state.active = state.active.filter(item => item !== id);
      else if (state.active.length >= PIVOT_MAX_MEASURES) refuse(`The pivot shows at most ${PIVOT_MAX_MEASURES} measures; turn one off first.`);
      else state.active.push(id);
    });
  }

  /** Order the rows by a measure of a column: first ascending, then descending, then as the server gave them. Local; reads nothing. */
  sortBy(measure: string, column: Path): Promise<void> {
    return this.change(state => {
      const same = state.sort?.measure === measure && pk(state.sort.path) === pk(column);
      state.sort = !same ? { measure, path: [...column], desc: false } : state.sort!.desc ? null : { ...state.sort!, desc: true };
    });
  }

  /** The records of one cell: the search, the row group and the column group. `undefined` when the cell holds none. */
  cellDomain(row: Path, col: Path): unknown[] | undefined {
    const cell = this.held.cells.get(cellKey(row, col));
    const rowDomain = this.held.domains[0].get(pk(row)), colDomain = this.held.domains[1].get(pk(col));
    if (!cell || !rowDomain || !colDomain) return undefined;
    return [...this.heldDomain, ...rowDomain, ...colDomain];
  }

  table(): PivotTable {
    const { axes, active, sort } = this.state, { held } = this;
    const { cells } = held; // this table keeps the cells it was built from, whatever the model does next
    const measures = active.map(id => this.config.measures.find(measure => measure.id === id)!);
    const sortMeasure = sort && this.config.measures.find(measure => measure.id === sort.measure);
    const value = (row: Path, col: Path, spec: string): number => cells.get(cellKey(row, col))?.values.get(spec) ?? 0;
    const children = (axis: 0 | 1, path: Path): Header[] => {
      const list = held.heads[axis].get(pk(path)) ?? [];
      if (axis !== 0 || !sort || !sortMeasure) return list;
      return [...list].sort((a, b) => (value([...path, a.key], sort.path, sortMeasure.spec) - value([...path, b.key], sort.path, sortMeasure.spec)) * (sort.desc ? -1 : 1));
    };
    const rows: RowHeader[] = [];
    const walkRows = (path: string[], label: string, depth: number): void => {
      const open = axes[0].open.has(pk(path));
      rows.push({ path, label, depth, open });
      if (open) for (const header of children(0, path)) walkRows([...path, header.key], header.label, depth + 1);
    };
    walkRows([], "Total", 0);
    const leaves: string[][] = [], nodes: { header: ColHeader; leaf: boolean }[] = [];
    const walkCols = (path: string[], label: string, depth: number): number => {
      const open = axes[1].open.has(pk(path)), kids = open ? children(1, path) : [];
      const header: ColHeader = { path, label, depth, open, span: 1, rowSpan: 1 };
      nodes.push({ header, leaf: !kids.length });
      if (!kids.length) { leaves.push(path); return 1; }
      header.span = kids.reduce((sum, kid) => sum + walkCols([...path, kid.key], kid.label, depth + 1), 0);
      return header.span;
    };
    walkCols([], "Total", 0);
    const height = Math.max(...nodes.map(node => node.header.depth)) + 1;
    const headerRows: ColHeader[][] = Array.from({ length: height }, () => []);
    for (const { header, leaf } of nodes) { if (leaf) header.rowSpan = height - header.depth; headerRows[header.depth]!.push(header); }
    return {
      rows, headerRows, leaves, totalColumn: leaves.length > 1, measures, sort, unavailable: this.stale,
      hasData: (cells.get(cellKey([], []))?.values.get("__count") ?? 0) > 0,
      cell: (row, col, measure) => cells.get(cellKey(row, col))?.values.get(measure.spec)
    };
  }
}

// ---- Download (the document is parsed by addons/web/src/pivot_export.rs) ----

/** One cell of the posted table. `kind` empty carries no value; float and monetary carry the decimals shown. */
export interface PivotExportCell { kind: "integer" | "float" | "monetary" | "empty"; value?: number; digits?: number; bold?: boolean }
/** A header cell; spans and bold are left out when they are the default (1, false). */
export interface PivotExportHeader { label: string; colspan?: number; rowspan?: number; bold?: boolean }
/** What `POST /erp/pivot/export` accepts: the table as drawn, header rows laid out like an HTML table, any depth. */
export interface PivotExportDocument {
  model: string; title: string;
  headers: PivotExportHeader[][];
  rows: { label: string; indent?: number; bold?: boolean; cells: PivotExportCell[] }[];
}

const headerCell = (label: string, colspan = 1, rowspan = 1, bold = false): PivotExportHeader =>
  ({ label, ...(colspan > 1 ? { colspan } : {}), ...(rowspan > 1 ? { rowspan } : {}), ...(bold ? { bold } : {}) });

/** The table on screen as the download route takes it. Refuses loudly when there is no table to download. */
export function pivotExportDocument(model: PivotModel, info: { model: string; title: string }): PivotExportDocument {
  const table = model.table();
  if (table.unavailable || !table.measures.length || !table.hasData) refuse("There is no table to download.");
  const m = table.measures.length;
  const { headerRows, leaves, totalColumn } = table;
  const columns: string[][] = totalColumn ? [...leaves, []] : leaves;
  const rowLevels = Array.from({ length: model.levelCount.row }, (_, depth) => model.levelAt("row", depth)!.label);
  const headers: PivotExportHeader[][] = headerRows.map((cells, depth) => {
    const line = cells.map(cell => headerCell(cell.path.length ? cell.label : "Total", cell.span * m, cell.rowSpan, !cell.path.length));
    if (depth === 0) line.unshift(headerCell(rowLevels.join(" / ") || "Records", 1, headerRows.length + 1));
    if (depth === 0 && totalColumn) line.push(headerCell("Total", m, headerRows.length, true));
    return line;
  });
  headers.push(columns.flatMap((_, index) => table.measures.map(measure => headerCell(measure.label, 1, 1, totalColumn && index === columns.length - 1))));
  const rows = table.rows.map(row => ({
    label: row.path.length ? row.label : "Total", ...(row.path.length ? { indent: row.depth } : { bold: true }),
    cells: columns.flatMap((col, index) => table.measures.map((measure): PivotExportCell => {
      const value = table.cell(row.path, col, measure);
      const bold = !row.path.length || !col.length || (totalColumn && index === columns.length - 1);
      const tail = bold ? { bold } : {};
      // As on screen: a group with records whose measure holds no value shows zero; a group with no records is blank.
      if (value === undefined) return { kind: "empty", ...tail };
      return { kind: measure.kind, value: value ?? 0, ...(measure.kind === "float" ? { digits: 2 } : {}), ...tail };
    }))
  }));
  return { model: info.model, title: info.title, headers, rows };
}

// ---- the address ----

export const PIVOT_PARAMS = ["pivot_rows", "pivot_cols", "pivot_measures", "pivot_open", "pivot_sort"] as const;
export type PivotParams = Partial<Record<(typeof PIVOT_PARAMS)[number], string>>;

/** The address parameters for a state. Only what differs from the arch is written: a pristine pivot keeps a clean address. */
export function snapshotParams(snapshot: PivotSnapshot): PivotParams {
  const params: PivotParams = {};
  if (!snapshot.layoutDefault) {
    if (snapshot.rows.length) params.pivot_rows = snapshot.rows.join(",");
    if (snapshot.cols.length) params.pivot_cols = snapshot.cols.join(",");
    params.pivot_open = JSON.stringify({ r: snapshot.open.row, c: snapshot.open.col });
  }
  if (!snapshot.measuresDefault && snapshot.measures.length) params.pivot_measures = snapshot.measures.join(",");
  if (!snapshot.sortDefault) params.pivot_sort = JSON.stringify(snapshot.sort ? [snapshot.sort.measure, snapshot.sort.desc ? "desc" : "asc", snapshot.sort.path] : []);
  return params;
}

/** The state an address describes, or the arch's when it holds none. `pivot_open` present means the layout is explicit (no `pivot_rows` is no rows). */
export function paramsState(params: PivotParams, config: PivotConfig): PivotInitial {
  const base = archState(config);
  const specs = (text: string | undefined, fallback: readonly PivotField[]): readonly PivotField[] => text === undefined ? fallback : text.split(",").map(spec => resolveSpec(spec, config.groupable));
  const explicit = params.pivot_open !== undefined;
  const state: PivotInitial = { ...base, rows: specs(params.pivot_rows, explicit ? [] : base.rows), cols: specs(params.pivot_cols, explicit ? [] : base.cols) };
  if (explicit) {
    const parsed = JSON.parse(params.pivot_open!) as { r?: unknown[][]; c?: unknown[][] };
    const keys = (paths: unknown[][] | undefined): string[][] => (paths ?? []).map(path => path.map(key => JSON.stringify(key)));
    state.open = { row: keys(parsed.r), col: keys(parsed.c) };
  }
  if (params.pivot_measures !== undefined) state.active = params.pivot_measures.split(",");
  if (params.pivot_sort !== undefined) {
    const parsed = JSON.parse(params.pivot_sort) as [string, string, unknown[]] | [];
    state.sort = parsed.length ? { measure: parsed[0], desc: parsed[1] === "desc", path: parsed[2].map(key => JSON.stringify(key)) } : null;
  }
  return state;
}
