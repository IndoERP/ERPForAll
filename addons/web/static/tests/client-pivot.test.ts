/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { HumanApp } from "../src/client/app.js";
import { ClientError } from "../src/client/transport.js";
import { configureClientTemplates } from "../src/client/registry.js";
import { describeViews, type View } from "../src/client/metadata.js";
import {
  applyContext, archState, decodePivotAnswer, decodePivotFields, paramsState, PIVOT_MAX_MEASURES, PivotModel, readPivotView, resolveSpec, snapshotParams, specOf,
  type PivotConfig, type PivotField, type PivotGroupItem, type PivotLimits, type PivotTable
} from "../src/client/pivot.js";
import { mountPivot, type PivotCellTarget } from "../src/client/pivot-view.js";
import { auditComponent } from "../../../../web/testing/axe.js";
import { answer, filterRecords, gated, recorder, records, type Rec } from "./pivot-fixture.js";

// ---- the arch as the server delivers it ----

const column = (type: string, label: string, extra: Record<string, unknown> = {}) =>
  ({ type, label, required: false, readonly: false, relation: null, relation_field: null, selection: null, ...extra });
const modelFields = {
  priority: column("selection", "Priority", { selection: [["normal", "Normal"], ["high", "High"]] }),
  state: column("selection", "State", { selection: [["draft", "Draft"], ["done", "Done"]] }),
  author_id: column("many2one", "Author", { relation: "t.author" }),
  size: column("integer", "Size"),
  weight: column("float", "Weight"),
  published: column("date", "Published"),
  name: column("char", "Name")
};
/** What `fields_get` answers (attributes string, type, groupable, selection, aggregator): crates/erp-orm/src/describe.rs. */
export const fieldsReply = {
  id: { string: "ID", type: "integer", groupable: true },
  name: { string: "Name", type: "char", groupable: true },
  priority: { string: "Priority", type: "selection", groupable: true, selection: [["normal", "Normal"], ["high", "High"]] },
  state: { string: "State", type: "selection", groupable: true, selection: [["draft", "Draft"], ["done", "Done"]] },
  author_id: { string: "Author", type: "many2one", groupable: true },
  size: { string: "Size", type: "integer", groupable: true, aggregator: "sum" },
  weight: { string: "Weight", type: "float", groupable: true, aggregator: "sum" },
  published: { string: "Published", type: "date", groupable: true },
  notes: { string: "Notes", type: "text", groupable: true },
  lines: { string: "Lines", type: "one2many", groupable: false }
};
const ref = (name: string) => ({ model: "t.note", name });
const pivotField = (name: string, role: string, label: string, interval?: string) => ({ kind: "pivot_field", field: ref(name), label, role, ...(interval ? { interval } : {}) });
type Flags = { disable_linking?: boolean; display_quantity?: boolean; default_order?: string };
const pivotDescription = (fields: unknown[], flags: Flags = {}) => ({
  wire_major: 1, registry_version: "r1", description_version: "d1", model: "t.note", fields: { "t.note": modelFields }, notices: [],
  views: [{ identity: { kind: "stored", id: 5 }, type: "pivot", schema_major: 1, renderer: "web.pivot", root: 0,
    nodes: [{ kind: "pivot", children: fields.map((_, index) => index + 1), disable_linking: flags.disable_linking ?? false, display_quantity: flags.display_quantity ?? false, ...(flags.default_order ? { default_order: flags.default_order } : {}) }, ...fields] }]
});
const view = (fields: unknown[], flags?: Flags): View => describeViews(pivotDescription(fields, flags), "t.note", "r1").views.get("pivot")!;
const catalogue = (items: PivotGroupItem[] = []) => ({ ...decodePivotFields(fieldsReply), items });
const config = (fields: unknown[], flags?: Flags, items?: PivotGroupItem[]): PivotConfig => readPivotView(view(fields, flags), catalogue(items));
const standard = () => [pivotField("priority", "row", "Priority"), pivotField("state", "col", "State"), pivotField("size", "measure", "Size")];
const LIMITS: PivotLimits = { maxGroups: 50, maxOpen: 20, maxCells: 400 };
const k = (value: unknown): string => JSON.stringify(value);
const field = (name: string, interval: PivotField["interval"] = null): PivotField => resolveSpec(interval ? `${name}:${interval}` : name, catalogue().groupable);

async function made(fields: unknown[] = standard(), options: { flags?: Flags; limits?: PivotLimits; source?: readonly Rec[]; domain?: () => unknown[]; initial?: ReturnType<typeof archState> } = {}) {
  const { call, requests } = recorder(options.source);
  const cfg = config(fields, options.flags);
  const model = new PivotModel(cfg, call, options.domain ?? (() => []), options.limits ?? LIMITS, options.initial);
  await model.load();
  return { model, requests, cfg };
}
const labels = (table: PivotTable): string[] => table.rows.map(row => `${"  ".repeat(row.depth)}${row.label}`);
const at = (table: PivotTable, row: string[], col: string[], measure = 0) => table.cell(row, col, table.measures[measure]!);
const rejects = async (promise: Promise<unknown>, kind: string, message: string | RegExp): Promise<void> => {
  const error = await promise.then(() => undefined, (caught: unknown) => caught as { kind: string; message: string });
  expect(error, "the call should have been refused").toBeDefined();
  expect(error!.kind).toBe(kind);
  expect(error!.message).toMatch(message);
};

// ---- arch, field list and context ----

/// (partial: the arch defaults of the flags and of the measures)
test("an arch with no flags or measures reads as linking allowed, no extra count and Count as the only measure", () => {
  const cfg = config([pivotField("priority", "row", "Priority")]);
  expect(cfg.disableLinking).toBe(false);
  expect(cfg.initial).toEqual(["__count"]);
  expect(cfg.rows.map(specOf)).toEqual(["priority"]);
  expect(cfg.cols).toEqual([]);
  expect(cfg.defaultOrder).toBeNull();
  expect(config(standard(), { display_quantity: true }).initial).toEqual(["__count", "size"]);
  expect(config(standard(), { disable_linking: true }).disableLinking).toBe(true);
});

/// (partial: several row and column fields in arch order, with date intervals; a date without an interval groups by month)
test("several row and column fields keep their order, a date field carries its interval and defaults to month", () => {
  const cfg = config([
    pivotField("priority", "row", "Priority"), pivotField("author_id", "row", "Author"), pivotField("published", "row", "Published", "quarter"),
    pivotField("state", "col", "State"), pivotField("published", "col", "Published"), pivotField("size", "measure", "Size"), pivotField("weight", "measure", "Weight")
  ], { default_order: "weight desc" });
  expect(cfg.rows.map(specOf)).toEqual(["priority", "author_id", "published:quarter"]);
  expect(cfg.cols.map(specOf)).toEqual(["state", "published:month"]);
  expect(cfg.initial).toEqual(["size", "weight"]);
  expect(cfg.defaultOrder).toEqual({ measure: "weight", desc: true });
});

/// (partial: the Measures menu lists every aggregatable field of the model, Count first, not only the arch's)
test("the measures menu offers Count and every aggregatable field, by label, whatever the arch declares", () => {
  const cfg = config([pivotField("priority", "row", "Priority")]);
  expect(cfg.measures.map(measure => [measure.id, measure.label, measure.spec, measure.kind])).toEqual([
    ["__count", "Count", "__count", "integer"], ["size", "Size", "size:sum", "integer"], ["weight", "Weight", "weight:sum", "float"]
  ]);
  // The arch's own label wins over the field's.
  expect(config([pivotField("size", "measure", "Pages")]).measures.find(measure => measure.id === "size")!.label).toBe("Pages");
});

/// (partial: what the client still refuses, by name)
test("an arch grouping on a float, a measure on a text field and a measure twice are refused", () => {
  expect(() => config([pivotField("weight", "row", "Weight")])).toThrowError(expect.objectContaining({ kind: "unsupported", message: "Grouping by Weight (a float field) is not available in the pivot view." }));
  expect(() => config([pivotField("name", "measure", "Name")])).toThrowError(expect.objectContaining({ kind: "unsupported" }));
  expect(() => config([pivotField("size", "measure", "Size"), pivotField("size", "measure", "Size again")])).toThrow("Invalid pivot measures: a field is declared twice.");
});

/// (a default order on a measure the caller cannot read has nothing to sort by: the pivot still opens)
test("a default_order that names a measure the field list does not offer sorts nothing instead of closing the pivot", () => {
  expect(config([pivotField("size", "measure", "Size")], { default_order: "weight asc" }).defaultOrder).toEqual({ measure: "weight", desc: false });
  expect(config([pivotField("size", "measure", "Size")], { default_order: "nothing asc" }).defaultOrder).toBeNull();
  expect(config([pivotField("size", "measure", "Size")], { default_order: "__count desc" }).defaultOrder).toEqual({ measure: "__count", desc: true });
});

/// (the context keys that set measures and groupings replace the arch's)
test("the context keys pivot_measures, pivot_row_groupby and pivot_column_groupby replace what the arch declares, and bad ones are refused", () => {
  const base = config(standard());
  expect(applyContext(base, { lang: "en_US" })).toBe(base);
  const changed = applyContext(base, { pivot_row_groupby: ["author_id", "published:year"], pivot_column_groupby: [], pivot_measures: ["weight", "__count"] });
  expect(changed.rows.map(specOf)).toEqual(["author_id", "published:year"]);
  expect(changed.cols).toEqual([]);
  expect(changed.initial).toEqual(["weight", "__count"]);
  expect(applyContext(base, { pivot_measures: ["weight"] }).rows.map(specOf)).toEqual(["priority"]); // a key left out keeps the arch
  expect(() => applyContext(base, { pivot_row_groupby: "state" })).toThrowError(expect.objectContaining({ kind: "validation", message: "The context key pivot_row_groupby must be a list of at most 6 names." }));
  expect(() => applyContext(base, { pivot_row_groupby: ["nothing"] })).toThrow("The pivot cannot group by nothing.");
  expect(() => applyContext(base, { pivot_measures: ["nothing"] })).toThrow("The context key pivot_measures names nothing, which is not a measure of this model.");
  expect(() => applyContext(base, { pivot_column_groupby: ["a", "b", "c", "d", "e", "f", "g"] })).toThrow("at most 6 names");
});

/// (a grouping from its text: dates carry an interval)
test("a grouping text resolves to a field with an interval only for dates", () => {
  const groupable = catalogue().groupable;
  expect(specOf(resolveSpec("published", groupable))).toBe("published:month");
  expect(specOf(resolveSpec("published:week", groupable))).toBe("published:week");
  expect(() => resolveSpec("state:month", groupable)).toThrow("State is not a date field, so state:month has no interval.");
  expect(() => resolveSpec("published:hour", groupable)).toThrow("names an interval that is not one of day, week, month, quarter, year");
  expect(() => resolveSpec("weight", groupable)).toThrow("The pivot cannot group by weight.");
  expect(() => resolveSpec(7, groupable)).toThrow("A pivot grouping must be a field name.");
});

/// (the field list: groupable kinds and aggregatable numbers only)
test("the field list keeps groupable fields in kinds the table groups by and numeric fields that sum", () => {
  const decoded = decodePivotFields(fieldsReply);
  expect([...decoded.groupable.keys()]).toEqual(["name", "priority", "state", "author_id", "size", "published"]); // not id, float weight, text notes, one2many lines
  expect(decoded.measures.map(measure => measure.id)).toEqual(["size", "weight"]);
  expect(() => decodePivotFields({ size: { string: "Size", type: "integer" } })).toThrow("Invalid description of field size.");
  expect(() => decodePivotFields([])).toThrow("Invalid field list.");
});

test("an answer that is not what the server sends is refused with the part that is wrong", () => {
  const rows = [field("priority")], axes = [rows, []] as const;
  const plans = [{ r: 1, c: 0 }];
  const row = (extra: Record<string, unknown> = {}) => ({ priority: "normal", __extra_domain: [], __count: 1, ...extra });
  const decode = (raw: unknown) => decodePivotAnswer(raw, plans, axes, ["__count"], LIMITS);
  expect(decode([[row()]]).cells.size).toBe(1);
  expect(() => decode([[row()], [row()]])).toThrow("Invalid pivot answer.");
  expect(() => decode([[row({ surprise: 1 })]])).toThrow("Invalid pivot group property.");
  expect(() => decode([[row(), row()]])).toThrow("Invalid pivot group list: duplicate group.");
  expect(() => decode([[row({ __count: -1 })]])).toThrow("Invalid pivot group count.");
  expect(() => decode([[row({ __extra_domain: "no" })]])).toThrow("Invalid pivot group domain.");
  expect(() => decode([[{ __extra_domain: [], __count: 1 }]])).toThrow("Invalid pivot group value of priority.");
  expect(() => decode([[row({ priority: "weird" })]])).toThrow("Invalid selection value of priority.");
  expect(() => decodePivotAnswer([[{ __extra_domain: [], __count: 1, "published:month": ["2026-01-01"] }]], [{ r: 1, c: 0 }], [[field("published")], []], ["__count"], LIMITS)).toThrow("Invalid group value of published.");
});

// ---- the first read ----

/// (one grouped call answers the whole table) (partial: every depth of the rows crossed with every depth of the columns)
test("the first read is one call whose sets cross every depth of the rows with every depth of the columns", async () => {
  const { model, requests } = await made([pivotField("priority", "row", "Priority"), pivotField("author_id", "row", "Author"), pivotField("state", "col", "State"), pivotField("size", "measure", "Size")]);
  expect(requests).toEqual([{
    groupingSets: [[], ["state"], ["priority"], ["priority", "state"], ["priority", "author_id"], ["priority", "author_id", "state"]],
    aggregates: ["__count", "size:sum"], domain: []
  }]);
  const table = model.table();
  expect(labels(table)).toEqual(["Total", "  Normal", "    Ada", "  High", "    Bo", "  None", "    None"]);
  expect(table.headerRows.map(cells => cells.map(cell => cell.label))).toEqual([["Total"], ["Draft", "Done"]]);
  expect(at(table, [k("normal"), "1"], [k("draft")])).toBe(10);
  expect(at(table, [k("normal"), "1"], [k("done")])).toBe(5);
  expect(at(table, [k("high"), "2"], [k("draft")])).toBe(10);
  expect(at(table, [k("high"), "2"], [k("done")])).toBeUndefined(); // no record: no value
  expect(at(table, [], [])).toBe(1525);
});

test("a table with no records says so and reads nothing more", async () => {
  const { model, requests } = await made(standard(), { source: [] });
  expect(model.table().hasData).toBe(false);
  expect(requests).toHaveLength(1);
});

// ---- expand and collapse ----

/// (expand a header by a field, one grouped read narrowed to that header; collapse reads nothing)
test("expanding a header reads once, narrowed to it; collapsing and expanding it again reads nothing", async () => {
  const { model, requests } = await made();
  const normal = [k("normal")];
  await model.expand("row", normal, field("author_id"));
  expect(requests).toHaveLength(2);
  expect(requests[1]).toEqual({
    groupingSets: [["priority", "author_id"], ["priority", "author_id", "state"]], aggregates: ["__count", "size:sum"], domain: [["priority", "=", "normal"]]
  });
  expect(labels(model.table())).toEqual(["Total", "  Normal", "    Ada", "  High", "  None"]);
  expect(at(model.table(), [k("normal"), "1"], [k("draft")])).toBe(10);
  // The other headers expand by the grouping chosen: no choice is asked again, and only that header is read.
  await model.expand("row", [k("high")]);
  expect(requests).toHaveLength(3);
  expect(requests[2]!.domain).toEqual([["priority", "=", "high"]]);
  expect(labels(model.table())).toEqual(["Total", "  Normal", "    Ada", "  High", "    Bo", "  None"]);
  await model.collapse("row", normal);
  expect(labels(model.table())).toEqual(["Total", "  Normal", "  High", "    Bo", "  None"]);
  await model.expand("row", normal);
  expect(requests).toHaveLength(3); // held already
  expect(labels(model.table())).toEqual(["Total", "  Normal", "    Ada", "  High", "    Bo", "  None"]);
});

/// (a column header expands by any field, crossed with every row depth on screen)
test("expanding a column header reads the columns under it against every row depth, narrowed to that header", async () => {
  const { model, requests } = await made();
  await model.expand("col", [k("draft")], field("published", "month"));
  expect(requests[1]).toEqual({
    groupingSets: [["state", "published:month"], ["priority", "state", "published:month"]], aggregates: ["__count", "size:sum"], domain: [["state", "=", "draft"]]
  });
  const table = model.table();
  expect(table.headerRows.map(cells => cells.map(cell => cell.label))).toEqual([["Total"], ["Draft", "Done"], ["January 2026", "February 2026", "April 2026"]]);
  expect(table.leaves.map(path => path.length)).toEqual([2, 2, 2, 1]); // three month leaves under Draft, then Done
  expect(table.headerRows[1]!.map(cell => [cell.span, cell.rowSpan])).toEqual([[3, 1], [1, 2]]); // a leaf spans down to the measure row
  expect(at(table, [k("normal")], [k("draft"), k("2026-01-01")])).toBe(10);
  expect(at(table, [k("high")], [k("draft"), k("2026-02-01")])).toBe(7);
  expect(at(table, [k("high")], [k("draft"), k("2026-04-01")])).toBe(3);
  expect(at(table, [], [k("draft"), k("2026-02-01")])).toBe(7);
});

/// (a header with no grouping left asks for one; the choice cannot skip a level)
test("a header with no grouping below it asks for a field and cannot skip a level", async () => {
  const { model, requests } = await made([pivotField("priority", "row", "Priority"), pivotField("size", "measure", "Size")]);
  await rejects(model.expand("row", [k("normal")]), "validation", "Choose a field to expand this header by.");
  await rejects(model.expand("row", [k("normal"), "1"], field("state")), "validation", "one level deeper");
  expect(requests).toHaveLength(1);
});

/// (the depth is bounded: six groupings on an axis)
test("a seventh level is refused and nothing is read, and an arch with seven is refused at the start", async () => {
  const six = [pivotField("priority", "row", "P"), pivotField("state", "row", "S"), pivotField("author_id", "row", "A"), pivotField("size", "row", "Z"), pivotField("published", "row", "Y", "year"), pivotField("published", "row", "M", "month")];
  const { model, requests } = await made([...six, pivotField("size", "measure", "Size")], { limits: { ...LIMITS, maxOpen: 100 } });
  const before = requests.length;
  const leaf = [k("normal"), k("draft"), "1", "10", k("2026-01-01"), k("2026-01-01")];
  await rejects(model.expand("row", leaf, field("published", "day")), "validation", "at most 6 fields on an axis");
  expect(requests).toHaveLength(before);
  const seven = [...six, pivotField("published", "row", "D", "day"), pivotField("size", "measure", "Size")];
  expect(() => new PivotModel(config(seven), recorder().call, () => [], LIMITS)).toThrow("at most 6 fields on an axis");
});

/// (collapsing the deepest open headers drops their grouping: the next expansion chooses again, and what was read under the old one never answers for the new one)
test("collapsing every header of the deepest level drops that grouping, and a new choice there is not answered from the old one's cells", async () => {
  // Authors 1 and 2 have the same ids as the sizes 1 and 2: the keys collide.
  const source: Rec[] = [
    { id: 1, priority: "normal", state: "draft", author: 1, size: 5, weight: 0, published: false, name: "A" },
    { id: 2, priority: "normal", state: "done", author: 2, size: 1, weight: 0, published: false, name: "B" }
  ];
  const { model } = await made([pivotField("priority", "row", "Priority"), pivotField("state", "col", "State"), pivotField("size", "measure", "Size")], { source });
  const normal = [k("normal")];
  await model.expand("row", normal, field("author_id"));
  expect(at(model.table(), [k("normal"), "1"], [k("draft")], 0)).toBe(5); // author 1, draft
  await model.collapse("row", normal);
  expect(model.levelAt("row", 1)).toBeUndefined(); // the author grouping is gone
  await model.expand("row", normal, field("size"));
  const table = model.table();
  expect(labels(table)).toEqual(["Total", "  Normal", "    1", "    5"]); // size 1 (author 2's record) and size 5
  expect(at(table, [k("normal"), "1"], [k("draft")])).toBeUndefined(); // no size-1 record is a draft; the author-1 cell must not answer
  expect(at(table, [k("normal"), "1"], [k("done")])).toBe(1);
  expect(at(table, [k("normal"), "5"], [k("draft")])).toBe(5);
});

/// (a header already grouped by one field cannot be given another)
test("a header whose depth is already grouped refuses a different field and says how to change it", async () => {
  const { model } = await made([pivotField("priority", "row", "Priority"), pivotField("state", "row", "State"), pivotField("size", "measure", "Size")]);
  await model.collapse("row", [k("normal")]);
  await rejects(model.expand("row", [k("normal")], field("author_id")), "validation", /already grouped by State; collapse the headers at that depth to choose another field/);
});

/// (expand all reads every set once; collapse all reads nothing, keeps the groupings, and a Total expands by them again)
test("expand all reads every level at once, collapse all folds both Totals without a read and the Total opens again without one", async () => {
  const { model, requests } = await made([pivotField("priority", "row", "Priority"), pivotField("state", "row", "State"), pivotField("size", "measure", "Size")]);
  await model.collapseAll();
  expect(requests).toHaveLength(1);
  expect(labels(model.table())).toEqual(["Total"]);
  expect(model.snapshot().layoutDefault).toBe(false);
  await model.expand("row", []); // the grouping is still declared: no choice, no read
  expect(requests).toHaveLength(1);
  expect(labels(model.table())).toEqual(["Total", "  Normal", "  High", "  None"]);
  await model.expandAll();
  expect(requests).toHaveLength(2);
  expect(requests[1]!.groupingSets).toEqual([[], ["priority"], ["priority", "state"]]);
  expect(labels(model.table())).toEqual(["Total", "  Normal", "    Draft", "    Done", "  High", "    Draft", "  None", "    Done"]);
  expect(model.snapshot().layoutDefault).toBe(true);
});

/// (flip swaps rows and columns without a read; the next partial read is narrowed to the right header)
test("flipping swaps the axes and every cell without a read, and a later expansion reads under the header it names", async () => {
  const { model, requests } = await made();
  const before = model.table();
  await model.sortBy("size", []);
  await model.flip();
  expect(requests).toHaveLength(1);
  const table = model.table();
  expect(labels(table)).toEqual(["Total", "  Draft", "  Done"]);
  expect(table.headerRows[1]!.map(cell => cell.label)).toEqual(["Normal", "High", "None"]);
  for (const row of ["normal", "high", false]) for (const col of ["draft", "done"]) expect(at(table, [k(col)], [k(row)])).toBe(at(before, [k(row)], [k(col)]));
  expect(at(table, [], [])).toBe(1525);
  expect(table.sort).toBeNull(); // a sort belongs to a column that no longer is one
  expect(model.snapshot().rows).toEqual(["state"]);
  await model.expand("row", [k("draft")], field("author_id"));
  expect(requests[1]).toEqual({ groupingSets: [["state", "author_id"], ["state", "author_id", "priority"]], aggregates: ["__count", "size:sum"], domain: [["state", "=", "draft"]] });
  expect(labels(model.table())).toEqual(["Total", "  Draft", "    Ada", "    Bo", "  Done"]);
});

// ---- sort ----

/// (sort the rows by a measure column: ascending, descending, none; ties keep the server's order)
test("a measure column sorts the rows ascending, then descending, then as the server gave them, with no read", async () => {
  const { model, requests } = await made();
  const order = () => labels(model.table());
  expect(order()).toEqual(["Total", "  Normal", "  High", "  None"]); // server order
  await model.sortBy("size", []);
  expect(order()).toEqual(["Total", "  High", "  Normal", "  None"]); // 10, 15, 1500
  await model.sortBy("size", []);
  expect(model.sort).toEqual({ measure: "size", path: [], desc: true });
  expect(order()).toEqual(["Total", "  None", "  Normal", "  High"]);
  await model.sortBy("size", []);
  expect(model.sort).toBeNull();
  expect(order()).toEqual(["Total", "  Normal", "  High", "  None"]);
  await model.sortBy("size", [k("draft")]); // the Draft column: Normal 10, High 10 (a tie), None has no draft record
  expect(order()).toEqual(["Total", "  None", "  Normal", "  High"]);
  expect(requests).toHaveLength(1);
});

test("every level is sorted among its siblings", async () => {
  const { model } = await made([pivotField("state", "row", "State"), pivotField("author_id", "row", "Author"), pivotField("size", "measure", "Size")]);
  expect(labels(model.table())).toEqual(["Total", "  Draft", "    Ada", "    Bo", "  Done", "    Ada", "    None"]);
  await model.sortBy("size", []);
  await model.sortBy("size", []);
  expect(labels(model.table())).toEqual(["Total", "  Done", "    None", "    Ada", "  Draft", "    Ada", "    Bo"]); // Done 1505 first; None 1500 before Ada 5; Draft's tie keeps Ada first
});

/// (default_order sorts the rows from the start)
test("default_order sorts the Total column before the user has touched anything and keeps sorting when its measure is hidden", async () => {
  const sorted = await made(standard(), { flags: { default_order: "size desc" } });
  expect(labels(sorted.model.table())).toEqual(["Total", "  None", "  Normal", "  High"]);
  expect(sorted.model.snapshot().sortDefault).toBe(true);
  const off = await made([pivotField("priority", "row", "Priority"), pivotField("size", "measure", "Size"), pivotField("weight", "measure", "Weight")], { flags: { default_order: "weight asc" } });
  await off.model.toggleMeasure("weight");
  expect(off.model.table().measures.map(measure => measure.id)).toEqual(["size"]);
  expect(labels(off.model.table())).toEqual(["Total", "  None", "  High", "  Normal"]); // weights 0, 3, 1234.75: the aggregate was read, only hidden
});

// ---- measures ----

/// (measures: Count is always read; turning one off is local; one on reads once; at most sixteen)
test("a measure is turned on with one read of the whole table and off or on again without one", async () => {
  const { model, requests } = await made();
  await model.toggleMeasure("__count");
  expect(requests).toHaveLength(1); // Count is read with every call
  expect(model.table().measures.map(measure => measure.id)).toEqual(["size", "__count"]);
  expect(at(model.table(), [], [], 1)).toBe(5);
  await model.toggleMeasure("weight");
  expect(requests).toHaveLength(2);
  expect(requests[1]!.aggregates).toEqual(["__count", "size:sum", "weight:sum"]);
  expect(requests[1]!.groupingSets).toEqual(requests[0]!.groupingSets); // the whole table again, one call
  expect(at(model.table(), [], [], 2)).toBe(1237.75);
  await model.toggleMeasure("weight");
  await model.toggleMeasure("weight");
  expect(requests).toHaveLength(2);
  await rejects(model.toggleMeasure("nothing"), "validation", "Unknown pivot measure nothing.");
});

test("sixteen measures are the most, and the seventeenth is refused with nothing read", async () => {
  const cfg = config(standard());
  const many = Array.from({ length: PIVOT_MAX_MEASURES + 1 }, (_, index) => ({ id: `m${index}`, label: `M${index}`, spec: `m${index}:sum`, kind: "integer" as const }));
  const crowded: PivotConfig = { ...cfg, measures: many, initial: many.slice(0, PIVOT_MAX_MEASURES).map(measure => measure.id) };
  const calls: unknown[] = [];
  const model = new PivotModel(crowded, async request => { calls.push(request); return [[{ __extra_domain: [], __count: 1, ...Object.fromEntries(many.slice(0, PIVOT_MAX_MEASURES).map(measure => [measure.spec, 1])) }], [], [], []]; }, () => [], LIMITS);
  await model.load();
  await rejects(model.toggleMeasure(`m${PIVOT_MAX_MEASURES}`), "validation", "at most 16 measures");
  expect(model.active).toHaveLength(PIVOT_MAX_MEASURES);
  expect(calls).toHaveLength(1);
  expect(() => new PivotModel(crowded, async () => [], () => [], LIMITS, { ...archState(crowded), active: many.map(measure => measure.id) })).toThrow("at most 16 measures");
});

// ---- the records behind a cell ----

/// (the domain of a cell: the search, the row group and the column group)
test("a cell's records are the search plus the row group plus the column group; a cell with no records has none", async () => {
  const { model } = await made(standard(), { domain: () => [["priority", "=", "normal"]] });
  expect(model.cellDomain([k("normal")], [k("draft")])).toEqual([["priority", "=", "normal"], ["priority", "=", "normal"], ["state", "=", "draft"]]);
  expect(model.cellDomain([], [])).toEqual([["priority", "=", "normal"]]);
  expect(model.cellDomain([k("normal")], [])).toEqual([["priority", "=", "normal"], ["priority", "=", "normal"]]);
  expect(model.cellDomain([k("high")], [k("done")])).toBeUndefined(); // no record, no cell
  const dated = await made([pivotField("published", "row", "Published", "month"), pivotField("size", "measure", "Size")]);
  expect(dated.model.cellDomain([k("2026-02-01")], [])).toEqual(["&", ["published", ">=", "2026-02-01"], ["published", "<", "2026-03-01"]]);
});

// ---- dates ----

/// (a date row is grouped by its interval: the interval travels in the grouping, the label is the server's)
test("a date grouping sends its interval, labels each bucket as the server does and expands by another interval", async () => {
  const { model, requests } = await made([pivotField("published", "row", "Published", "month"), pivotField("size", "measure", "Size")]);
  expect(requests[0]!.groupingSets).toEqual([[], ["published:month"]]);
  expect(labels(model.table())).toEqual(["Total", "  January 2026", "  February 2026", "  April 2026", "  None"]);
  expect(at(model.table(), [k("2026-02-01")], [])).toBe(12);
  await model.expand("row", [k("2026-02-01")], field("published", "day"));
  expect(requests[1]).toEqual({ groupingSets: [["published:month", "published:day"]], aggregates: ["__count", "size:sum"], domain: ["&", ["published", ">=", "2026-02-01"], ["published", "<", "2026-03-01"]] });
  expect(labels(model.table())).toEqual(["Total", "  January 2026", "  February 2026", "    10 Feb 2026", "    20 Feb 2026", "  April 2026", "  None"]);
  const quarters = await made([pivotField("published", "col", "Published", "quarter"), pivotField("size", "measure", "Size")]);
  expect(quarters.model.table().headerRows[1]!.map(cell => cell.label)).toEqual(["Q1 2026", "Q2 2026", "None"]);
});

// ---- the bounds ----

/// (bounds from settings: groups per level, expanded headers, cells; each refusal names the setting and changes nothing)
test("a grouping with more groups than the setting allows is refused with a message that names the setting", async () => {
  const { call } = recorder();
  const model = new PivotModel(config(standard()), call, () => [], { ...LIMITS, maxGroups: 2 });
  await rejects(model.load(), "validation", /Grouping by Priority returns more than 2 groups, the most the pivot view shows \(set in Settings: Pivot view, most groups\)/);
  expect(model.table().unavailable).toBe(true);
});

test("more expanded headers than the setting allows are refused before anything is read, and the table stays as it was", async () => {
  const { model, requests } = await made([pivotField("priority", "row", "Priority"), pivotField("size", "measure", "Size")], { limits: { ...LIMITS, maxOpen: 2 } });
  await model.expand("row", [k("normal")], field("author_id")); // root and Normal: two
  const before = labels(model.table());
  await rejects(model.expand("row", [k("high")]), "validation", /More than 2 headers would stay expanded.*set in Settings: Pivot view, most headers that may stay expanded/);
  expect(requests).toHaveLength(2);
  expect(labels(model.table())).toEqual(before);
  const crowded = new PivotModel(config([pivotField("priority", "row", "P"), pivotField("state", "row", "S"), pivotField("size", "measure", "Size")]), recorder().call, () => [], { ...LIMITS, maxOpen: 2 });
  await rejects(crowded.load(), "validation", /more than 2 expanded headers/);
});

test("a table with more cells than the setting allows is refused", async () => {
  const model = new PivotModel(config(standard()), recorder().call, () => [], { ...LIMITS, maxCells: 8 });
  await rejects(model.load(), "validation", /more than 8 cells, the most the pivot view holds \(set in Settings: Pivot view, most cells\)/);
});

// ---- answers that arrive late, together or after the view is gone ----

const tick = (): Promise<void> => new Promise(resolve => setTimeout(resolve, 0));

test("two searches in flight end in the table of the later one, whichever answer arrives first", async () => {
  let source = records.filter(record => record.priority === "normal");
  const { call, requests, release, overlapped } = gated(() => source);
  const model = new PivotModel(config(standard()), call, () => [], LIMITS);
  const first = model.load();
  await vi.waitFor(() => expect(requests).toHaveLength(1));
  source = records.filter(record => record.priority === "high");
  const second = model.load();
  await tick();
  expect(requests).toHaveLength(1); // the second read waits for the first: it is not sent while the first is in flight
  release(0);
  await first;
  await vi.waitFor(() => expect(requests).toHaveLength(2));
  release(1);
  await second;
  expect(overlapped()).toBe(false);
  expect(at(model.table(), [], [])).toBe(10);
});

/// (a queued expansion reads under the search the table on screen was built from)
test("a queued expansion reads under the search of the table it extends, and the queued new search reads under the new one", async () => {
  let domain: unknown[] = [["priority", "=", "normal"]];
  const { call, requests, release } = gated(() => records);
  const model = new PivotModel(config(standard()), call, () => domain, LIMITS);
  const opened = model.load(); await vi.waitFor(() => expect(requests).toHaveLength(1)); release(0); await opened;
  const measure = model.toggleMeasure("weight"); // a full read, under the old search
  await vi.waitFor(() => expect(requests).toHaveLength(2));
  const expand = model.expand("row", [k("normal")], field("author_id")); // queued behind it
  domain = [["priority", "=", "high"]]; // the user submits a search meanwhile
  const search = model.load();
  release(1); await measure;
  await vi.waitFor(() => expect(requests).toHaveLength(3));
  expect(requests[1]!.domain).toEqual([["priority", "=", "normal"]]);
  expect(requests[2]!.domain).toEqual([["priority", "=", "normal"], ["priority", "=", "normal"]]); // merged into the table read under the old search, narrowed to its header
  release(2); await expand;
  await vi.waitFor(() => expect(requests).toHaveLength(4));
  expect(requests[3]!.domain).toEqual([["priority", "=", "high"]]);
  release(3); await search;
});

test("a job still waiting or in flight when the view is disposed changes nothing", async () => {
  const { call, requests, release } = gated(() => records);
  const model = new PivotModel(config(standard()), call, () => [], LIMITS);
  const opened = model.load(); await vi.waitFor(() => expect(requests).toHaveLength(1)); release(0); await opened;
  const before = labels(model.table());
  const collapse = model.collapse("row", []), expand = model.expand("col", [k("draft")], field("author_id"));
  model.dispose();
  await collapse; await expand;
  expect(labels(model.table())).toEqual(before);
  expect(requests).toHaveLength(1);
});

test("a failed search read marks the table unavailable and the next job reads everything again", async () => {
  let fail = false;
  const requests: unknown[] = [];
  const model = new PivotModel(config(standard()), async request => { requests.push(request); if (fail) throw new Error("The server is busy."); return answer({ domain: request.domain, grouping_sets: request.groupingSets, aggregates: request.aggregates }); }, () => [], LIMITS);
  await model.load();
  fail = true;
  await expect(model.load()).rejects.toThrow("The server is busy.");
  expect(model.table().unavailable).toBe(true);
  fail = false; requests.length = 0;
  await model.collapse("row", []);
  expect(model.table().unavailable).toBe(false);
  expect(requests).toEqual([{ groupingSets: [[], ["state"]], aggregates: ["__count", "size:sum"], domain: [] }]); // all of it again, not one partial read on figures of the old search
  expect(at(model.table(), [], [])).toBe(1525);
});

test("a search that runs again keeps the expanded headers that still exist and drops the ones that do not", async () => {
  let domain: unknown[] = [];
  const { call } = recorder();
  const model = new PivotModel(config(standard()), call, () => domain, LIMITS);
  await model.load();
  await model.expand("row", [k("normal")], field("author_id"));
  await model.expand("row", [k("high")]);
  expect(labels(model.table())).toEqual(["Total", "  Normal", "    Ada", "  High", "    Bo", "  None"]);
  domain = [["priority", "=", "normal"]]; // High no longer exists
  await model.load();
  expect(labels(model.table())).toEqual(["Total", "  Normal", "    Ada"]);
  expect(model.snapshot().open.row).toEqual([[], ["normal"]]); // the open set was cut to what exists
});

// ---- the address ----

/// (state in the address: nothing for a pristine pivot, everything that differs otherwise, and it restores the same table)
test("the state is kept in the address and a reload restores the same table with one read", async () => {
  const { model } = await made();
  expect(snapshotParams(model.snapshot())).toEqual({});
  await model.expand("row", [k("normal")], field("author_id"));
  await model.expand("col", [k("draft")], field("published", "month"));
  await model.toggleMeasure("weight");
  await model.sortBy("size", [k("draft")]);
  const params = snapshotParams(model.snapshot());
  expect(Object.keys(params).sort()).toEqual(["pivot_cols", "pivot_measures", "pivot_open", "pivot_rows", "pivot_sort"]);
  expect(params.pivot_rows).toBe("priority,author_id");
  expect(params.pivot_cols).toBe("state,published:month");
  expect(params.pivot_measures).toBe("size,weight");
  expect(JSON.parse(params.pivot_open!)).toEqual({ r: [[], ["normal"]], c: [[], ["draft"]] });
  expect(JSON.parse(params.pivot_sort!)).toEqual(["size", "asc", ["draft"]]);

  const { call, requests } = recorder();
  const again = new PivotModel(config(standard()), call, () => [], LIMITS, paramsState(params, config(standard())));
  await again.load();
  expect(requests).toHaveLength(1);
  expect(requests[0]!.groupingSets).toHaveLength(9); // rows 0..2 by columns 0..2
  const [a, b] = [model.table(), again.table()];
  expect(labels(b)).toEqual(labels(a));
  expect(b.headerRows.map(cells => cells.map(cell => cell.label))).toEqual(a.headerRows.map(cells => cells.map(cell => cell.label)));
  for (const row of a.rows) for (const col of a.leaves) for (const index of [0, 1]) expect(at(b, row.path, col, index)).toEqual(at(a, row.path, col, index));
  expect(b.sort).toEqual(a.sort);
  expect(snapshotParams(again.snapshot())).toEqual(params);
});

test("a state with every header folded and no groupings is kept: an open list that is present means the layout is explicit", async () => {
  const { model } = await made();
  await model.collapse("row", []); await model.collapse("col", []);
  const params = snapshotParams(model.snapshot());
  expect(params).toEqual({ pivot_open: '{"r":[],"c":[]}' });
  const restored = paramsState(params, config(standard()));
  expect([restored.rows.length, restored.cols.length]).toEqual([0, 0]);
  const again = new PivotModel(config(standard()), recorder().call, () => [], LIMITS, restored);
  await again.load();
  expect(labels(again.table())).toEqual(["Total"]);
  expect(snapshotParams(again.snapshot())).toEqual(params);
});

test("an address that names an unknown grouping, a measure that does not exist or a header deeper than its groupings is refused", () => {
  const cfg = config(standard());
  expect(() => paramsState({ pivot_rows: "nothing", pivot_open: '{"r":[[]]}' }, cfg)).toThrow("The pivot cannot group by nothing.");
  expect(() => new PivotModel(cfg, recorder().call, () => [], LIMITS, paramsState({ pivot_measures: "nothing" }, cfg))).toThrow("Unknown pivot measure nothing.");
  expect(() => new PivotModel(cfg, recorder().call, () => [], LIMITS, paramsState({ pivot_rows: "priority", pivot_open: '{"r":[[],["normal"]]}' }, cfg))).toThrow("The address expands a header deeper than its groupings.");
  expect(paramsState({ pivot_sort: "[]" }, cfg).sort).toBeNull();
});

test("the model asks the application whether it can keep a state before it reads anything for it", async () => {
  const { call, requests } = recorder();
  const accept = vi.fn((snapshot: { rows: string[] }) => { if (snapshot.rows.length > 1) throw new Error("This table is too large to keep in the page address."); });
  const model = new PivotModel(config(standard()), call, () => [], LIMITS, undefined, accept);
  await model.load();
  await expect(model.expand("row", [k("normal")], field("author_id"))).rejects.toThrow("too large to keep in the page address");
  expect(requests).toHaveLength(1);
  expect(model.snapshot().rows).toEqual(["priority"]);
});

// ---- the table, as a user meets it ----

const american = { decimal_point: ".", thousands_sep: ",", grouping: [3, 0] };
const german = { decimal_point: ",", thousands_sep: ".", grouping: [3, 0] };
afterEach(() => document.body.replaceChildren());
const items: PivotGroupItem[] = [{ id: "groupby_author", description: "By author", field: { ...field("author_id") } }, { id: "groupby_month", description: "By month", field: field("published", "month") }];

async function mounted(fields: unknown[] = standard(), options: { locale?: typeof american; source?: readonly Rec[]; flags?: Flags; open?: (target: PivotCellTarget) => Promise<void>; changed?: () => void } = {}) {
  const { call, requests } = recorder(options.source);
  const cfg = config(fields, options.flags, items);
  const model = new PivotModel(cfg, call, () => [], LIMITS);
  await model.load();
  const jobs: Promise<void>[] = [];
  const opened: PivotCellTarget[] = [];
  const pivot = mountPivot({ model, locale: options.locale ?? american, title: "Notes", run: job => { jobs.push(job()); },
    openCell: options.open ?? (async target => { opened.push(target); }), changed: options.changed });
  document.body.append(pivot.element);
  const settle = async (): Promise<void> => { await Promise.all(jobs.splice(0)); };
  return { pivot, model, requests, settle, root: pivot.element, opened };
}
const cells = (root: HTMLElement, selector: string): string[] => [...root.querySelectorAll(selector)].map(node => node.textContent!.trim());
const press = (target: Element, key: string): void => { target.dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true, cancelable: true })); };
const header = (root: HTMLElement, axis: "row" | "col", path: string[]): HTMLButtonElement =>
  [...root.querySelectorAll<HTMLButtonElement>("[data-pivot-header]")].find(node => node.dataset["pivotHeader"] === `${axis}:${path.join("\u0000")}`)!;

/// (partial: the toolbar group, the Measures menu, flip, expand all and collapse all; the download button is another slice's: a slot is left for it)
test("the table renders totals, group headers and measure headers with the user's number format, and the toolbar leaves a slot for the download", async () => {
  const { root } = await mounted(standard(), { locale: german });
  expect(root.querySelector("[role=toolbar]")!.getAttribute("aria-label")).toBe("Main actions");
  expect([...root.querySelectorAll(".pivot-tool")].map(button => button.textContent)).toEqual(["Flip axis", "Expand all", "Collapse all"]);
  expect(root.querySelector(".pivot-toolbar-end")!.childElementCount).toBe(0);
  expect(root.querySelector("caption")!.textContent).toBe("Notes, rows by Priority, columns by State");
  expect(cells(root, ".pivot-head-groups th")).toEqual(["Draft", "Done"]);
  expect(cells(root, ".pivot-head-measures th")).toEqual(["Size", "Size", "Size"]);
  expect(cells(root, ".pivot-row-header")).toEqual(["Total", "Normal", "High", "None"]);
  expect(cells(root, ".pivot-row-total .pivot-cell")).toEqual(["20", "1.505", "1.525"]); // grouping and decimal marks of the user's locale
  expect(cells(root, "tbody tr:nth-child(4) .pivot-cell")).toEqual(["", "1.500", "1.500"]); // no record of None is a draft: blank
  expect(root.querySelectorAll("th[scope=row]")).toHaveLength(4);
  expect(root.querySelectorAll("tbody tr:nth-child(2) .pivot-cell.pivot-total")).toHaveLength(1);
});

test("a float measure shows two decimals in the user's locale", async () => {
  const { root } = await mounted([pivotField("priority", "row", "Priority"), pivotField("weight", "measure", "Weight")], { locale: german });
  expect(cells(root, ".pivot-row-total .pivot-cell")).toEqual(["1.237,75"]);
  expect(cells(root, "tbody tr:nth-child(2) .pivot-cell")).toEqual(["1.234,75"]);
  expect(cells(root, "tbody tr:nth-child(4) .pivot-cell")).toEqual(["0,00"]);
});

/// (every header is a button with aria-expanded; a click folds an open header, expands a folded one by the next grouping or asks for a field)
test("every header is a button with aria-expanded: a click folds, expands or asks for a field, and the pressed header keeps focus", async () => {
  const { root, requests, settle } = await mounted();
  expect(header(root, "row", []).getAttribute("aria-label")).toBe("Total, rows grouped by Priority");
  expect(header(root, "col", []).getAttribute("aria-label")).toBe("Total, columns grouped by State");
  expect(header(root, "row", [k("normal")]).getAttribute("aria-expanded")).toBe("false");
  expect(header(root, "row", [k("normal")]).getAttribute("aria-haspopup")).toBe("menu");
  expect(header(root, "row", []).getAttribute("aria-haspopup")).toBeNull();
  requests.length = 0;

  header(root, "row", []).focus();
  header(root, "row", []).click();
  await settle();
  expect(header(root, "row", []).getAttribute("aria-expanded")).toBe("false");
  expect(document.activeElement).toBe(header(root, "row", []));
  expect(root.querySelectorAll("tbody tr")).toHaveLength(1);
  expect(requests).toEqual([]);
  // Nothing is grouped by any more: the Total asks for a field, as in the reference.
  expect(header(root, "row", []).getAttribute("aria-haspopup")).toBe("menu");
  header(root, "row", []).click();
  [...root.querySelectorAll<HTMLElement>(".pivot-field-menu [role=menuitem]")].find(item => item.textContent === "Priority")!.click();
  await settle();
  expect(root.querySelectorAll("tbody tr")).toHaveLength(4);
  expect(requests).toHaveLength(1);
  expect(requests[0]!.groupingSets).toEqual([["priority"], ["priority", "state"]]);

  header(root, "col", []).click();
  await settle();
  expect(root.querySelector(".pivot-head-groups")).toBeNull();
  expect(cells(root, ".pivot-row-total .pivot-cell")).toEqual(["1,525"]);
});

/// (the expand menu: the search view's group-by items first, then the model's groupable fields; a date field has its intervals)
test("a header with no grouping below it opens a menu of the search view's items and the groupable fields, and the choice expands it with one read", async () => {
  const { root, requests, settle } = await mounted();
  requests.length = 0;
  const normal = header(root, "row", [k("normal")]);
  normal.focus(); normal.click();
  const menu = root.querySelector<HTMLElement>(".pivot-field-menu")!;
  expect(menu.classList.contains("open")).toBe(true);
  const entries = [...menu.querySelectorAll<HTMLElement>("[role=menuitem]")].filter(item => !item.classList.contains("pivot-interval-item")).map(item => item.textContent);
  expect(entries).toEqual(["By author", "By month", "Author", "Name", "Priority", "Published", "Size", "State"]);
  expect(menu.querySelectorAll(".pivot-menu-heading")).toHaveLength(2);
  const author = [...menu.querySelectorAll<HTMLElement>("[role=menuitem]")].find(item => item.textContent === "Author")!;
  author.click();
  await settle();
  expect(menu.classList.contains("open")).toBe(false);
  expect(requests).toHaveLength(1);
  expect(requests[0]!.domain).toEqual([["priority", "=", "normal"]]);
  expect(cells(root, ".pivot-row-header")).toEqual(["Total", "Normal", "Ada", "High", "None"]);
  expect(header(root, "row", [k("normal")]).getAttribute("aria-expanded")).toBe("true");
  expect(document.activeElement).toBe(header(root, "row", [k("normal")]));
  expect(header(root, "row", [k("high")]).getAttribute("aria-haspopup")).toBeNull(); // the author grouping is declared now: High expands by it directly
});

test("a date field in the menu offers Year, Quarter, Month, Week and Day, and the interval chosen goes to the server in the grouping", async () => {
  const { root, requests, settle } = await mounted();
  requests.length = 0;
  header(root, "col", [k("draft")]).click();
  const menu = root.querySelector<HTMLElement>(".pivot-field-menu")!;
  const parent = [...menu.querySelectorAll<HTMLElement>("[role=menuitem]")].find(item => item.textContent === "Published")!;
  expect(parent.getAttribute("aria-haspopup")).toBe("menu");
  expect(parent.getAttribute("aria-expanded")).toBe("false");
  const intervals = menu.querySelector<HTMLElement>(".pivot-interval-group")!;
  expect(intervals.hidden).toBe(true);
  parent.click();
  expect(parent.getAttribute("aria-expanded")).toBe("true");
  expect([...intervals.querySelectorAll("[role=menuitem]")].map(item => item.textContent)).toEqual(["Year", "Quarter", "Month", "Week", "Day"]);
  [...intervals.querySelectorAll<HTMLElement>("[role=menuitem]")].find(item => item.textContent === "Quarter")!.click();
  await settle();
  expect(requests[0]!.groupingSets).toEqual([["state", "published:quarter"], ["priority", "state", "published:quarter"]]);
  expect(cells(root, "thead tr:nth-child(2) th")).toEqual(["Draft", "Done"]);
  expect(cells(root, "thead tr:nth-child(3) th")).toEqual(["Q1 2026", "Q2 2026"]);
});

test("a search view's group-by item expands the header by its field and interval", async () => {
  const { root, requests, settle } = await mounted();
  requests.length = 0;
  header(root, "row", [k("high")]).click();
  [...root.querySelectorAll<HTMLElement>(".pivot-field-menu [role=menuitem]")].find(item => item.textContent === "By month")!.click();
  await settle();
  expect(requests[0]!.groupingSets).toEqual([["priority", "published:month"], ["priority", "published:month", "state"]]);
  expect(cells(root, ".pivot-row-header")).toEqual(["Total", "Normal", "High", "February 2026", "April 2026", "None"]);
});

/// (keyboard: ArrowRight expands, ArrowLeft folds; a header with nothing below it opens the menu, Escape closes it and returns to the header; focus stays where it was)
test("the arrow keys expand and fold a header, Escape closes the menu to the header, and focus stays on the header after the redraw", async () => {
  const { root, settle, requests } = await mounted([pivotField("priority", "row", "Priority"), pivotField("state", "row", "State"), pivotField("size", "measure", "Size")]);
  const normal = () => header(root, "row", [k("normal")]);
  requests.length = 0;
  normal().focus();
  press(normal(), "ArrowLeft");
  await settle();
  expect(normal().getAttribute("aria-expanded")).toBe("false");
  expect(document.activeElement).toBe(normal());
  expect(cells(root, ".pivot-row-header")).toEqual(["Total", "Normal", "High", "Draft", "None", "Done"]);
  press(normal(), "ArrowRight"); // the next grouping is declared: no menu, and it was read with the first call
  await settle();
  expect(cells(root, ".pivot-row-header")).toEqual(["Total", "Normal", "Draft", "Done", "High", "Draft", "None", "Done"]);
  expect(document.activeElement).toBe(normal());
  expect(requests).toHaveLength(0);
  // Nothing is declared under Draft: ArrowRight opens the menu, Escape closes it and focus returns to the header.
  const draft = header(root, "row", [k("normal"), k("draft")]);
  draft.focus();
  press(draft, "ArrowRight");
  const menu = root.querySelector<HTMLElement>(".pivot-field-menu")!;
  expect(menu.classList.contains("open")).toBe(true);
  expect(menu.contains(document.activeElement)).toBe(true);
  const first = document.activeElement;
  press(document.activeElement!, "ArrowDown");
  expect(document.activeElement).not.toBe(first);
  expect(menu.contains(document.activeElement)).toBe(true);
  press(document.activeElement!, "Escape");
  expect(menu.classList.contains("open")).toBe(false);
  expect(document.activeElement).toBe(header(root, "row", [k("normal"), k("draft")]));
  expect(requests).toHaveLength(0);
});

test("an expansion that finishes late does not take focus from where the user went", async () => {
  const { call, requests, release } = gated(() => records);
  const model = new PivotModel(config(standard()), call, () => [], LIMITS);
  const opened = model.load(); await vi.waitFor(() => expect(requests).toHaveLength(1)); release(0); await opened;
  const jobs: Promise<void>[] = [];
  const pivot = mountPivot({ model, locale: american, title: "Notes", run: job => { jobs.push(job()); } });
  document.body.append(pivot.element);
  const search = document.createElement("input");
  document.body.append(search);
  header(pivot.element, "row", [k("normal")]).click();
  pivot.element.querySelector<HTMLElement>(".pivot-field-menu [role=menuitem]:not(.pivot-field-parent)")!.click();
  await vi.waitFor(() => expect(requests).toHaveLength(2));
  search.focus();
  release(1);
  await Promise.all(jobs);
  expect(cells(pivot.element, ".pivot-row-header")).toContain("Ada");
  expect(document.activeElement).toBe(search);
});

/// (without any field to group by, a header is plain text)
test("without a grouping and without groupable fields a Total is plain text", async () => {
  const cfg: PivotConfig = { ...config([pivotField("size", "measure", "Size")]), groupable: new Map() };
  const model = new PivotModel(cfg, recorder().call, () => [], LIMITS);
  await model.load();
  const pivot = mountPivot({ model, locale: american, title: "Notes", run: job => { void job(); } });
  expect(pivot.element.querySelector(".pivot-toggle")).toBeNull();
  expect(cells(pivot.element, ".pivot-row-header")).toEqual(["Total"]);
  expect(cells(pivot.element, ".pivot-cell")).toEqual(["1,525"]);
});

/// (sort control: the measure header is a button, aria-sort follows, the click cycles ascending, descending, none)
test("a measure header sorts the rows: aria-sort and the order follow the clicks, and the third click clears it", async () => {
  const { root, settle, requests } = await mounted();
  requests.length = 0;
  const heads = () => [...root.querySelectorAll<HTMLElement>(".pivot-head-measures th")];
  const done = () => heads()[1]!.querySelector<HTMLButtonElement>(".pivot-sort")!; // the Done column: Normal 5, High none, None 1,500
  expect(heads().map(head => head.getAttribute("aria-sort"))).toEqual(["none", "none", "none"]);
  done().click(); await settle();
  expect(heads().map(head => head.getAttribute("aria-sort"))).toEqual(["none", "ascending", "none"]);
  expect(cells(root, ".pivot-row-header")).toEqual(["Total", "High", "Normal", "None"]);
  expect(done().getAttribute("aria-label")).toBe("Size, sort rows, sorted ascending, press for descending");
  done().click(); await settle();
  expect(heads()[1]!.getAttribute("aria-sort")).toBe("descending");
  expect(cells(root, ".pivot-row-header")).toEqual(["Total", "None", "Normal", "High"]);
  done().click(); await settle();
  expect(heads().map(head => head.getAttribute("aria-sort"))).toEqual(["none", "none", "none"]);
  expect(cells(root, ".pivot-row-header")).toEqual(["Total", "Normal", "High", "None"]);
  expect(requests).toEqual([]);
});

/// (a value cell opens its records; a cell with no records is blank and not a link; disable_linking turns links off)
test("a value cell is a link to its records, an empty cell is not, and an arch with disable_linking has no links", async () => {
  const { root, opened, settle } = await mounted(standard());
  const links = [...root.querySelectorAll<HTMLButtonElement>(".pivot-cell-link")];
  expect(links.map(link => link.textContent)).toEqual(["20", "1,505", "1,525", "10", "5", "15", "10", "10", "1,500", "1,500"]); // Total; Normal; High (no Done); None (no Draft)
  const row = root.querySelector<HTMLTableRowElement>("tbody tr:nth-child(3)")!; // High: draft 10, done none
  expect([...row.querySelectorAll("td")].map(cell => cell.classList.contains("pivot-empty"))).toEqual([false, true, false]);
  expect(row.querySelectorAll("td")[1]!.querySelector("button")).toBeNull();
  const normalDraft = root.querySelector<HTMLButtonElement>("tbody tr:nth-child(2) td:nth-child(2) .pivot-cell-link")!;
  expect(normalDraft.getAttribute("aria-label")).toBe("10 Size, Normal, Draft: open the records");
  normalDraft.click(); await settle();
  expect(opened).toEqual([{ domain: [["priority", "=", "normal"], ["state", "=", "draft"]], rowLabels: ["Normal"], colLabels: ["Draft"] }]);
  const none = await mounted(standard(), { flags: { disable_linking: true } });
  expect(none.root.querySelectorAll(".pivot-cell-link")).toHaveLength(0);
  expect(cells(none.root, ".pivot-row-total .pivot-cell")).toEqual(["20", "1,505", "1,525"]);
});

/// (the value cells are one tab stop; the arrow keys move between them, over empty cells)
test("the value cells are one tab stop and the arrow keys move between them, skipping empty cells", async () => {
  const { root } = await mounted();
  const stops = [...root.querySelectorAll<HTMLButtonElement>(".pivot-cell-link")].filter(link => link.tabIndex === 0);
  expect(stops).toHaveLength(1);
  // Columns count the row header: 2 is Draft, 3 is Done, 4 is Total. Rows: 1 Total, 2 Normal, 3 High (no Done), 4 None (no Draft).
  const link = (row: number, col: number) => root.querySelector<HTMLButtonElement>(`tbody tr:nth-child(${row}) td:nth-child(${col}) .pivot-cell-link`)!;
  expect(stops[0]).toBe(link(1, 2));
  link(2, 2).focus();
  press(link(2, 2), "ArrowRight"); expect(document.activeElement).toBe(link(2, 3));
  press(link(2, 3), "ArrowDown"); expect(document.activeElement).toBe(link(4, 3)); // High has no Done: skipped
  press(link(4, 3), "ArrowLeft"); expect(document.activeElement).toBe(link(4, 3)); // None has no Draft and nothing is left of it: focus stays
  press(link(4, 3), "ArrowUp"); expect(document.activeElement).toBe(link(2, 3));
  press(link(2, 3), "ArrowUp"); expect(document.activeElement).toBe(link(1, 3));
  expect(link(1, 3).tabIndex).toBe(0); // the one tab stop follows the focus
  expect(link(2, 2).tabIndex).toBe(-1);
  expect([...root.querySelectorAll<HTMLButtonElement>(".pivot-cell-link")].filter(item => item.tabIndex === 0)).toHaveLength(1);
});

/// (a group with records whose measure holds no value shows zero; a group with no records is blank)
test("a cell whose group has records but whose aggregate is null shows zero, and one with no group is blank", async () => {
  const { call } = recorder();
  const model = new PivotModel(config([pivotField("priority", "row", "Priority"), pivotField("weight", "measure", "Weight")]), async request => {
    const rows = await call(request) as Record<string, unknown>[][];
    return rows.map(set => set.map(row => row["priority"] === "high" ? { ...row, "weight:sum": null } : row));
  }, () => [], LIMITS);
  await model.load();
  const pivot = mountPivot({ model, locale: american, title: "Notes", run: job => { void job(); } });
  expect(cells(pivot.element, "tbody tr:nth-child(3) .pivot-cell")).toEqual(["0.00"]);
  expect(at(model.table(), [k("high")], [])).toBeNull();
  expect(at(model.table(), [k("nobody")], [])).toBeUndefined();
});

/// (flip, expand all and collapse all from the toolbar)
test("Flip axis, Expand all and Collapse all work from the toolbar, and are disabled when they could change nothing", async () => {
  const { root, settle, requests } = await mounted([pivotField("priority", "row", "Priority"), pivotField("state", "row", "State"), pivotField("size", "measure", "Size")]);
  const [flip, expand, collapse] = [...root.querySelectorAll<HTMLButtonElement>(".pivot-tool")];
  requests.length = 0;
  flip!.click(); await settle();
  expect(cells(root, "thead tr:nth-child(2) th")).toEqual(["Normal", "High", "None"]);
  expect(cells(root, "thead tr:nth-child(3) th")).toEqual(["Draft", "Done", "Draft", "Done"]);
  expect(cells(root, ".pivot-row-header")).toEqual(["Total"]);
  expect(cells(root, "thead .pivot-corner")).toEqual(["Records"]);
  expect(requests).toEqual([]);
  flip!.click(); await settle();
  expect(cells(root, ".pivot-row-header")).toEqual(["Total", "Normal", "Draft", "Done", "High", "Draft", "None", "Done"]);
  collapse!.click(); await settle();
  expect(cells(root, ".pivot-row-header")).toEqual(["Total"]);
  expect(collapse!.disabled).toBe(true);
  expect(expand!.disabled).toBe(false);
  expect(requests).toEqual([]);
  expand!.click(); await settle();
  expect(cells(root, ".pivot-row-header")).toEqual(["Total", "Normal", "Draft", "Done", "High", "Draft", "None", "Done"]);
  expect(collapse!.disabled).toBe(false);
  expect(requests).toHaveLength(1);
  const bare = await mounted([pivotField("size", "measure", "Size")]);
  expect(bare.root.querySelectorAll<HTMLButtonElement>(".pivot-tool")[1]!.disabled).toBe(true); // no grouping, nothing to expand
});

/// (the changed hook: the application keeps the address current after every change the user made)
test("every change the user makes tells the application once the table is drawn, so it can write the address", async () => {
  const seen: string[] = [];
  const { root, settle, model } = await mounted(standard(), { changed: () => { seen.push(JSON.stringify(model.snapshot().rows)); } });
  header(root, "row", [k("normal")]).click();
  [...root.querySelectorAll<HTMLElement>(".pivot-field-menu [role=menuitem]")].find(item => item.textContent === "Author")!.click();
  await settle();
  root.querySelector<HTMLButtonElement>(".pivot-tool")!.click(); await settle();
  expect(seen).toEqual(['["priority","author_id"]', '["state"]']);
});

/// (partial: the Measures menu is a set of checkable items, Count and every aggregatable field)
test("the Measures menu toggles measures: off draws without a read, on reads the new aggregate once", async () => {
  const { root, requests, settle } = await mounted();
  root.querySelector<HTMLButtonElement>(".pivot-measures-toggle")!.click();
  const items = () => [...root.querySelectorAll<HTMLElement>("[role=menuitemcheckbox]")];
  expect(items().map(item => [item.textContent, item.getAttribute("aria-checked")])).toEqual([["Count", "false"], ["Size", "true"], ["Weight", "false"]]);
  requests.length = 0;
  items()[0]!.click(); await settle();
  expect(requests).toHaveLength(0); // Count is always read
  expect(cells(root, ".pivot-head-measures th").slice(0, 2)).toEqual(["Size", "Count"]);
  expect(cells(root, ".pivot-row-total .pivot-cell")).toEqual(["20", "3", "1,505", "2", "1,525", "5"]); // Size then Count in each column
  items()[2]!.click(); await settle();
  expect(requests).toHaveLength(1);
  expect(requests[0]!.aggregates).toEqual(["__count", "size:sum", "weight:sum"]);
  root.querySelector<HTMLElement>('[data-measure="size"]')!.click(); await settle();
  root.querySelector<HTMLElement>('[data-measure="__count"]')!.click(); await settle();
  root.querySelector<HTMLElement>('[data-measure="weight"]')!.click(); await settle();
  expect(root.querySelector(".pivot-table")).toBeNull();
  expect(root.querySelector(".pivot-status")!.textContent).toBe("No measure is selected. Choose one in the Measures menu.");
});

/// (the toolbar buttons are disabled when there is nothing to show)
test("a table with no records says so and the toolbar buttons that act on it are disabled", async () => {
  const { root } = await mounted(standard(), { source: [] });
  expect([...root.querySelectorAll<HTMLButtonElement>(".pivot-tool")].map(button => button.disabled)).toEqual([true, true, true]);
  expect(root.querySelector(".pivot-table")).toBeNull();
  expect(root.querySelector(".pivot-status")!.textContent).toBe("There is nothing to show: no record matches the search.");
});

test("a failed read draws the table as unavailable, never the old figures", async () => {
  const jobs: Promise<void>[] = [];
  let fail = false;
  const model = new PivotModel(config(standard()), async request => { if (fail) throw new Error("The server is busy."); return answer({ domain: request.domain, grouping_sets: request.groupingSets, aggregates: request.aggregates }); }, () => [], LIMITS);
  await model.load();
  const pivot = mountPivot({ model, locale: american, title: "Notes", run: job => { jobs.push(job().catch(() => undefined)); } });
  document.body.append(pivot.element);
  fail = true;
  pivot.element.querySelector<HTMLButtonElement>(".pivot-measures-toggle")!.click();
  pivot.element.querySelector<HTMLElement>('[data-measure="weight"]')!.click();
  await Promise.all(jobs);
  expect(pivot.element.querySelector(".pivot-table")).toBeNull();
  expect(pivot.element.querySelector(".pivot-status")!.textContent).toBe("Unable to load the table. Adjust the search or reload the page.");
});

test("a disposed pivot draws nothing more", async () => {
  const { pivot, model, root } = await mounted();
  const before = root.innerHTML;
  pivot.dispose();
  await model.collapse("row", []);
  pivot.render();
  expect(root.innerHTML).toBe(before);
});

/// (accessibility: the nested table, its menu and its headers pass the audit and are scoped)
test("the pivot passes the accessibility audit with nested headers and the expand menu open, and its header cells are scoped", async () => {
  const { root, settle } = await mounted([pivotField("priority", "row", "Priority"), pivotField("author_id", "row", "Author"), pivotField("state", "col", "State"), pivotField("size", "measure", "Size")]);
  header(root, "col", [k("draft")]).click();
  await settle();
  const report = await auditComponent(root);
  expect(report.violations.map(violation => violation.id)).toEqual([]);
  for (const cell of root.querySelectorAll<HTMLTableCellElement>("thead th")) {
    if (cell.classList.contains("pivot-measure-header")) expect(cell.getAttribute("scope")).toBe("col");
    else expect(cell.getAttribute("scope")).toBe(cell.colSpan > 1 ? "colgroup" : "col");
  }
  expect(root.querySelectorAll("thead th[scope=colgroup]").length).toBeGreaterThan(0);
  void tick;
});

// ---- through the application: the switcher, the address, the settings and the records behind a cell ----

const bootstrap = {
  wire_major: 1, registry_version: "r1", asset_version: "a1",
  identity: { uid: 2, name: "Operator", internal: true },
  companies: [{ id: 1, name: "Company" }], current_company_id: 1, allowed_company_ids: [1], active_company_ids: [1],
  locale: { lang: "en_US", tz: "UTC", date_format: "%m/%d/%Y", time_format: "%H:%M:%S", ...american },
  csrf: { logout: "test-token-not-secret" }
};
const modifiers = { readonly: null, required: null, invisible: null };
const str = (value: string) => ({ kind: "literal", value: { kind: "str", value } });
const groupByContext = (spec: string) => ({ schema: 1, nodes: [str("group_by"), str(spec), { kind: "dict", items: [[0, 1]] }] });
const full = {
  ...pivotDescription(standard()), model: "t.note",
  views: [
    { identity: { kind: "stored", id: 2 }, type: "list", schema_major: 1, renderer: "web.list", root: 0, nodes: [{ kind: "list", children: [1] }, { kind: "field", force_save: false, field: ref("name"), widget: "char", options: {}, modifiers, label: "Name" }] },
    pivotDescription(standard()).views[0]!,
    { identity: { kind: "stored", id: 3 }, type: "search", schema_major: 1, renderer: "web.search", root: 0, nodes: [
      { kind: "search", children: [1, 2] },
      { kind: "search_filter", name: "by_author", label: "By author", context: groupByContext("author_id") },
      { kind: "search_filter", name: "by_month", label: "By month", context: groupByContext("published:month") }
    ] }
  ]
};
/** The settings the server would answer for the pivot bounds (crates/erp-registry `SettingDecl`, addons/web/src/lib.rs). */
const settings: Record<string, number> = { "web.pivot_max_groups": 50, "web.pivot_max_open_headers": 20, "web.pivot_max_cells": 400 };
let served: readonly Rec[] = records;

beforeEach(() => {
  served = records;
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value(this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value(this: HTMLDialogElement) { this.open = false; } });
  configureClientTemplates([{ name: "web.HumanControlPanel", render: () => {
    const root = document.createElement("section");
    root.setAttribute("data-control-panel", "");
    for (const key of ["breadcrumbs", "search", "actions", "extension", "tools"]) { const node = document.createElement("div"); node.setAttribute(`data-control-${key}`, ""); root.append(node); }
    return root;
  } }]);
});
let app: HumanApp | undefined;
const closeApp = (): void => { app?.dispose(); app = undefined; document.body.replaceChildren(); };
afterEach(() => { app?.dispose(); app = undefined; vi.restoreAllMocks(); vi.unstubAllGlobals(); history.replaceState(null, "", "/"); });

type Call = { method: string; body: Record<string, unknown> };
const reply = (value: unknown, status = 200) => new Response(JSON.stringify(value), { status, headers: { "Content-Type": "application/json" } });
let holdReads: Promise<void> | undefined; // while set, grouped reads wait for it
let failReads = false; // while set, grouped reads fail
let noSearchView = false; // while set, the model has no default search view: the server refuses the request for one (addons/base/src/ir_ui_view/select.rs)
afterEach(() => { failReads = false; holdReads = undefined; noSearchView = false; });
async function startApp(path: string, options: { tolerant?: boolean } = {}) {
  history.replaceState(null, "", path);
  const calls: Call[] = [];
  vi.stubGlobal("fetch", vi.fn(async (url: string, init: RequestInit) => {
    const method = url.split("/").pop()!, body = init.body ? JSON.parse(String(init.body)) as Record<string, unknown> : {};
    calls.push({ method, body });
    if (method === "erp_load_web_menus") return reply({ wire_major: 1, registry_version: "r1", roots: [10], nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }] });
    if (method === "erp_load_web_action" || method === "erp_load_web_action_views") {
      const action = { wire_major: 1, id: 20, name: "Notes", model: "t.note", target: "current", views: [{ id: 2, type: "list" }, { id: 5, type: "pivot" }], domain: null, context: null };
      return reply(method === "erp_load_web_action_views" ? { action, views: full } : action);
    }
    if (method === "get_views") {
      const asked = body["views"] as unknown[][];
      if (noSearchView && asked.some(item => item[1] === "search")) return reply({ message: "No default view of type 'search' could be found! (generated default views come with the model)" }, 422);
      return reply(full);
    }
    if (method === "fields_get") {
      if (!Array.isArray(body["attributes"]) || !body["attributes"].every(name => typeof name === "string")) return reply({ message: "attributes must be a list of strings" }, 422);
      return reply(fieldsReply);
    }
    if (method === "erp_read_values") {
      // addons/base/src/ir_config_setting.rs `read_values`: a key no installed addon declares is refused like a denied one.
      const keys = body["keys"] as string[];
      const scope = body["scope"] as { kind?: string };
      if (scope?.kind !== "global" || !Array.isArray(keys)) return reply({ message: "invalid declared setting or value" }, 422);
      const unknown = keys.find(key => !(key in settings));
      if (unknown) return reply({ message: "Setting is unavailable or access is denied." }, 403);
      return reply({ wire_major: 1, scope: { kind: "global" }, revision: "0", values: keys.map(key => ({ key, value: settings[key], source: "default" })) });
    }
    if (method === "formatted_read_grouping_sets") {
      if (failReads) return reply({ message: "The server is busy." }, 500);
      await holdReads;
      return reply(answer(body, served));
    }
    if (method === "web_read_group") {
      // the list's grouped read: the first grouping only, as the server does (crates/erp-orm/src/web/groups.rs)
      const rows = answer({ domain: body["domain"], grouping_sets: [[(body["groupby"] as string[])[0]]], aggregates: ["__count"] }, served)[0]!;
      return reply({ groups: rows, length: rows.length });
    }
    if (method === "web_search_read") {
      const kept = filterRecords(body["domain"] as unknown[], served);
      return reply({ length: kept.length, records: kept.map(record => ({ id: record.id, name: record.name })) });
    }
    return reply([]);
  }));
  const root = document.createElement("div");
  for (const region of ["header", "navigation", "notice", "content"]) { const node = document.createElement("div"); node.setAttribute(`data-client-${region}`, ""); root.append(node); }
  document.body.append(root);
  app = new HumanApp(root, bootstrap);
  let failure: { message?: string } | undefined;
  if (options.tolerant) await app.start().catch((error: unknown) => { failure = error as { message?: string }; }); else await app.start();
  return { root, calls, failure: () => failure, of: (method: string) => calls.filter(call => call.method === method) };
}
const query = (): URLSearchParams => new URL(window.location.href).searchParams;
const pivotParams = (): string[] => [...query().keys()].filter(key => key.startsWith("pivot_")).sort();
const ready = (root: HTMLElement) => vi.waitFor(() => expect(root.querySelector(".pivot-table")).not.toBeNull());
const choose = (root: HTMLElement, text: string): void => { [...root.querySelectorAll<HTMLElement>(".pivot-field-menu [role=menuitem]")].find(item => item.textContent === text)!.click(); };

/// (the pivot reached from the view switcher reads its bounds from Settings, the field list once, and the table with one grouped call)
test("a model with a declared pivot view is reached from the view switcher: settings, fields and one grouped call", async () => {
  const { root, of } = await startApp("/erp/action-20");
  expect(root.querySelector(".list-table")).not.toBeNull();
  expect([...root.querySelectorAll(".view-btn")].map(button => button.getAttribute("aria-label"))).toEqual(["list view", "pivot view"]);
  root.querySelector<HTMLButtonElement>('.view-btn[aria-label="pivot view"]')!.click();
  await ready(root);
  expect(query().get("view_type")).toBe("pivot");
  expect(pivotParams()).toEqual([]); // a pristine pivot keeps a clean address
  expect(of("erp_read_values")).toHaveLength(1);
  expect(of("erp_read_values")[0]!.body).toMatchObject({ scope: { kind: "global" }, keys: ["web.pivot_max_groups", "web.pivot_max_open_headers", "web.pivot_max_cells"] });
  expect(of("fields_get")[0]!.body["attributes"]).toEqual(["string", "type", "groupable", "selection", "aggregator"]);
  expect(of("formatted_read_grouping_sets")).toHaveLength(1);
  expect(of("formatted_read_grouping_sets")[0]!.body).toMatchObject({ domain: [], grouping_sets: [[], ["state"], ["priority"], ["priority", "state"]], aggregates: ["__count", "size:sum"] });
  expect(root.querySelector('.view-btn[aria-label="pivot view"]')!.getAttribute("aria-current")).toBe("page");
  expect(cells(root, ".pivot-row-total .pivot-cell")).toEqual(["20", "1,505", "1,525"]);
  root.querySelector<HTMLButtonElement>('.view-btn[aria-label="list view"]')!.click();
  await vi.waitFor(() => expect(root.querySelector(".list-table")).not.toBeNull());
  expect(root.querySelector(".pivot-table")).toBeNull();
});

/// (the state is in the address; a reload restores it with one read; the settings are read again)
test("expanding writes the address, and opening that address again restores the same table with one grouped read", async () => {
  const first = await startApp("/erp/action-20?view_type=pivot");
  await ready(first.root);
  header(first.root, "row", [k("normal")]).click();
  choose(first.root, "Author");
  await vi.waitFor(() => expect(cells(first.root, ".pivot-row-header")).toEqual(["Total", "Normal", "Ada", "High", "None"]));
  expect(pivotParams()).toEqual(["pivot_cols", "pivot_open", "pivot_rows"]);
  expect(query().get("pivot_rows")).toBe("priority,author_id");
  expect(query().get("pivot_cols")).toBe("state");
  expect(JSON.parse(query().get("pivot_open")!)).toEqual({ r: [[], ["normal"]], c: [[]] });
  expect(query().get("view_type")).toBe("pivot");
  const address = window.location.search;
  const text = cells(first.root, "tbody .pivot-cell");
  closeApp();
  const again = await startApp(`/erp/action-20${address}`);
  await ready(again.root);
  expect(again.of("formatted_read_grouping_sets")).toHaveLength(1);
  expect(again.of("formatted_read_grouping_sets")[0]!.body["grouping_sets"]).toHaveLength(6); // two row depths by two column depths, with their totals
  expect(cells(again.root, ".pivot-row-header")).toEqual(["Total", "Normal", "Ada", "High", "None"]);
  expect(cells(again.root, "tbody .pivot-cell")).toEqual(text);
  expect(window.location.search).toBe(address);
});

/// (measures, sort and flip are in the address too)
test("the measures, the sort and a flip are kept in the address and come back after a reload", async () => {
  const first = await startApp("/erp/action-20?view_type=pivot");
  await ready(first.root);
  first.root.querySelector<HTMLButtonElement>(".pivot-measures-toggle")!.click();
  first.root.querySelector<HTMLElement>('[data-measure="weight"]')!.click();
  await vi.waitFor(() => expect(first.root.querySelectorAll(".pivot-head-measures th")).toHaveLength(6));
  expect(query().get("pivot_measures")).toBe("size,weight");
  first.root.querySelector<HTMLButtonElement>(".pivot-head-measures th .pivot-sort")!.click();
  await vi.waitFor(() => expect(query().get("pivot_sort")).toBe('["size","asc",["draft"]]'));
  first.root.querySelector<HTMLButtonElement>(".pivot-tool")!.click(); // flip: the sort belongs to a column that is a row now
  await vi.waitFor(() => expect(cells(first.root, ".pivot-row-header")).toEqual(["Total", "Draft", "Done"]));
  expect(query().get("pivot_rows")).toBe("state");
  expect(query().get("pivot_cols")).toBe("priority");
  expect(query().get("pivot_sort")).toBeNull(); // no sort is what the arch declares: nothing to keep
  const address = window.location.search;
  closeApp();
  const again = await startApp(`/erp/action-20${address}`);
  await ready(again.root);
  expect(cells(again.root, ".pivot-row-header")).toEqual(["Total", "Draft", "Done"]);
  expect(cells(again.root, ".pivot-head-measures th").slice(0, 2)).toEqual(["Size", "Weight"]);
  expect(again.of("formatted_read_grouping_sets")[0]!.body["aggregates"]).toEqual(["__count", "size:sum", "weight:sum"]);
});

/// (a cell opens the list of its records, with the search and both groups; the breadcrumb back restores the pivot as it was left)
test("a cell opens its records as a list and the way back restores the pivot with its expansion", async () => {
  const { root, of } = await startApp("/erp/action-20?view_type=pivot");
  await ready(root);
  header(root, "row", [k("normal")]).click();
  choose(root, "Author");
  await vi.waitFor(() => expect(cells(root, ".pivot-row-header")).toContain("Ada"));
  const pivotAddress = window.location.href;
  const cell = root.querySelector<HTMLButtonElement>('.pivot-cell-link[aria-label^="10 Size, Normal, Ada, Draft"]')!;
  cell.click();
  await vi.waitFor(() => expect(root.querySelector(".list-table")).not.toBeNull());
  expect(query().get("method_action")).not.toBeNull();
  expect(query().get("view_type")).toBe("list");
  expect(pivotParams()).toEqual([]);
  const read = of("web_search_read").at(-1)!.body["domain"];
  expect(read).toEqual(["&", ["priority", "=", "normal"], ["author_id", "=", 1], ["state", "=", "draft"]]); // the row group (two conditions joined) and the column group
  expect(cells(root, ".list-table tbody tr")).toHaveLength(1);
  expect(root.querySelector(".list-table tbody")!.textContent).toContain("Alpha");
  const crumb = root.querySelector<HTMLButtonElement>(".breadcrumb-origin")!;
  expect(crumb.textContent).toBe("Notes");
  expect(root.querySelector("h1.breadcrumb-current")!.textContent).toBe("Normal / Ada / Draft");
  crumb.click();
  await ready(root);
  expect(window.location.href).toBe(pivotAddress);
  expect(cells(root, ".pivot-row-header")).toEqual(["Total", "Normal", "Ada", "High", "None"]);
});

/// (disable_linking in the arch: the cells are not links in the application either)
test("a cell with disable_linking in the arch is not a link in the application either", async () => {
  const flagged = { ...full, views: full.views.map(item => item.type === "pivot" ? { ...item, nodes: [{ ...(item.nodes[0] as object), disable_linking: true }, ...item.nodes.slice(1)] } : item) };
  const original = full.views;
  full.views = flagged.views as typeof full.views;
  try {
    const { root } = await startApp("/erp/action-20?view_type=pivot");
    await ready(root);
    expect(root.querySelectorAll(".pivot-cell-link")).toHaveLength(0);
  } finally { full.views = original; }
});

/// (the search bar narrows the pivot; its group-by is not offered; the expanded headers that still exist stay)
test("a search narrows the table with one more grouped call, keeps the expansion that still exists and writes the address", async () => {
  const { root, of } = await startApp("/erp/action-20?view_type=pivot");
  await ready(root);
  expect(root.querySelector(".erp-groupby-section")).toBeNull(); // the pivot owns its groupings
  header(root, "row", [k("normal")]).click();
  choose(root, "Author");
  await vi.waitFor(() => expect(cells(root, ".pivot-row-header")).toContain("Ada"));
  const before = of("formatted_read_grouping_sets").length;
  const input = root.querySelector<HTMLInputElement>(".search-input")!;
  input.value = "Normal";
  root.querySelector<HTMLFormElement>(".search-form")!.dispatchEvent(new Event("submit", { cancelable: true }));
  await vi.waitFor(() => expect(of("formatted_read_grouping_sets")).toHaveLength(before + 1));
  expect(of("formatted_read_grouping_sets").at(-1)!.body["domain"]).toEqual([["priority", "=", "normal"]]);
  await vi.waitFor(() => expect(cells(root, ".pivot-row-header")).toEqual(["Total", "Normal", "Ada"]));
  expect(cells(root, ".pivot-row-total .pivot-cell")).toEqual(["10", "5", "15"]); // only the two normal-priority records remain
  expect(JSON.parse(query().get("pivot_open")!).r).toEqual([[], ["normal"]]);
  expect(query().get("search_facets")).not.toBeNull();
});

test("a search whose read fails leaves the pivot saying it is unavailable, not the old figures", async () => {
  const { root, of } = await startApp("/erp/action-20?view_type=pivot");
  await ready(root);
  failReads = true;
  root.querySelector<HTMLInputElement>(".search-input")!.value = "Normal";
  root.querySelector<HTMLFormElement>(".search-form")!.dispatchEvent(new Event("submit", { cancelable: true }));
  await vi.waitFor(() => expect(of("formatted_read_grouping_sets")).toHaveLength(2));
  await vi.waitFor(() => expect(root.querySelector(".pivot-status")!.textContent).toBe("Unable to load the table. Adjust the search or reload the page."));
  expect(root.querySelector(".pivot-table")).toBeNull();
});

test("a search read that answers after the user went back to the list does not put the pivot address back", async () => {
  const { root, of } = await startApp("/erp/action-20?view_type=pivot");
  await ready(root);
  let open!: () => void;
  holdReads = new Promise<void>(resolve => { open = resolve; });
  root.querySelector<HTMLInputElement>(".search-input")!.value = "Normal";
  root.querySelector<HTMLFormElement>(".search-form")!.dispatchEvent(new Event("submit", { cancelable: true }));
  await vi.waitFor(() => expect(of("formatted_read_grouping_sets")).toHaveLength(2));
  root.querySelector<HTMLButtonElement>('.view-btn[aria-label="list view"]')!.click();
  await vi.waitFor(() => expect(root.querySelector(".list-table")).not.toBeNull());
  const address = window.location.href;
  holdReads = undefined; open();
  await new Promise(resolve => setTimeout(resolve, 20));
  expect(window.location.href).toBe(address);
  expect(query().get("view_type")).not.toBe("pivot");
  expect(pivotParams()).toEqual([]);
  expect(root.querySelector(".pivot-table")).toBeNull();
});

/// (going back to the list drops the pivot's parameters from the address)
test("the view switcher leaves the pivot's own parameters behind", async () => {
  const { root } = await startApp("/erp/action-20?view_type=pivot");
  await ready(root);
  header(root, "row", [k("normal")]).click();
  choose(root, "Author");
  await vi.waitFor(() => expect(pivotParams().length).toBeGreaterThan(0));
  root.querySelector<HTMLButtonElement>('.view-btn[aria-label="list view"]')!.click();
  await vi.waitFor(() => expect(root.querySelector(".list-table")).not.toBeNull());
  expect(pivotParams()).toEqual([]);
});

/// (an address the pivot cannot hold is refused, never half-read)
test("a malformed pivot parameter, an unknown grouping and a record id are refused and nothing is read", async () => {
  for (const path of [
    "/erp/action-20?view_type=pivot&pivot_rows=Bad%20Field",
    "/erp/action-20?view_type=pivot&pivot_rows=nothing&pivot_open=%7B%22r%22%3A%5B%5D%7D",
    "/erp/action-20?view_type=pivot&pivot_measures=__count%2Cnothing",
    "/erp/action-20?view_type=pivot&pivot_open=%7B%22r%22%3A%5B%5B%22a%22%5D%5D%7D",
    "/erp/action-20?view_type=pivot&id=5"
  ]) {
    const refused = await startApp(path, { tolerant: true });
    expect(refused.root.querySelector(".pivot-table"), path).toBeNull();
    expect(refused.of("formatted_read_grouping_sets"), path).toHaveLength(0);
    closeApp();
  }
});

/// (a setting that is not a whole number is a server fault: the pivot does not open on a guess)
test("a settings answer that is not a whole number leaves the pivot closed with nothing read", async () => {
  settings["web.pivot_max_groups"] = 0;
  try {
    const { root, of, failure } = await startApp("/erp/action-20?view_type=pivot", { tolerant: true });
    expect(root.querySelector(".pivot-table")).toBeNull();
    expect(of("formatted_read_grouping_sets")).toHaveLength(0);
    expect(failure()?.message).toBe("Invalid pivot setting web.pivot_max_groups.");
  } finally { settings["web.pivot_max_groups"] = 50; }
});

/// (the bounds come from Settings: a lower setting is the bound the user meets)
test("the groups bound is the setting's: with it lowered the first read is refused with the setting named", async () => {
  settings["web.pivot_max_groups"] = 2;
  try {
    const { root, of, failure } = await startApp("/erp/action-20?view_type=pivot", { tolerant: true });
    expect(of("formatted_read_grouping_sets")).toHaveLength(1);
    expect(root.querySelector(".pivot-table")).toBeNull();
    expect(failure()?.message).toBe("Grouping by Priority returns more than 2 groups, the most the pivot view shows (set in Settings: Pivot view, most groups). Pick another field or narrow the search.");
  } finally { settings["web.pivot_max_groups"] = 50; }
});

/// (a state too large for the address is refused with a message that says what to do, and nothing is read for it)
test("an expansion whose state would not fit in the address is refused with a clear message and reads nothing", async () => {
  const long = (letter: string): string => letter.repeat(400);
  served = ["a", "b", "c", "d"].map((letter, index): Rec => ({ id: index + 1, priority: "normal", state: "draft", author: 1, size: 1, weight: 0, published: false, name: long(letter) }));
  const { root, of } = await startApp("/erp/action-20?view_type=pivot");
  await ready(root);
  header(root, "row", [k("normal")]).click();
  choose(root, "Name");
  await vi.waitFor(() => expect(cells(root, ".pivot-row-header").length).toBe(6));
  let reads = of("formatted_read_grouping_sets").length;
  for (const letter of ["a", "b", "c", "d"]) {
    const target = header(root, "row", [k("normal"), k(long(letter))]);
    if (!target) break;
    target.click();
    choose(root, "State");
    await new Promise(resolve => setTimeout(resolve, 20));
    if (document.body.textContent!.includes("too large to keep in the page address")) break;
    reads = of("formatted_read_grouping_sets").length;
  }
  expect(document.body.textContent).toContain("This table is too large to keep in the page address. Collapse some headers or show fewer measures first.");
  expect(of("formatted_read_grouping_sets")).toHaveLength(reads); // the refused expansion read nothing
  expect(window.location.href.length).toBeLessThanOrEqual(2048 + window.location.origin.length);
});

// ---- cycle 9 review fixes (M1 to M3, S1, S3) ----

/// (partial: a change that answers after the user left the pivot leaves the next page's address alone)
test("an expansion that answers after the user went to the list does not put the pivot address back", async () => {
  const { root, of } = await startApp("/erp/action-20?view_type=pivot");
  await ready(root);
  let open!: () => void;
  holdReads = new Promise<void>(resolve => { open = resolve; });
  header(root, "row", [k("normal")]).click();
  choose(root, "Author");
  await vi.waitFor(() => expect(of("formatted_read_grouping_sets")).toHaveLength(2));
  root.querySelector<HTMLButtonElement>('.view-btn[aria-label="list view"]')!.click();
  await vi.waitFor(() => expect(root.querySelector(".list-table")).not.toBeNull());
  const address = window.location.href;
  holdReads = undefined; open();
  await new Promise(resolve => setTimeout(resolve, 20));
  expect(window.location.href).toBe(address);
  expect(pivotParams()).toEqual([]);
});

/// (partial: a bound broken after the read of a new search marks the table unavailable)
test("a new search whose answer breaks the cell bound leaves the table unavailable, not the old search's figures", async () => {
  let domain: unknown[] = [["priority", "=", "normal"]];
  const { model } = await made(standard(), { limits: { maxGroups: 50, maxOpen: 20, maxCells: 6 }, domain: () => domain });
  expect(model.table().unavailable).toBe(false);
  domain = [];
  await rejects(model.load(), "validation", /more than 6 cells/);
  expect(model.table().unavailable).toBe(true);
});

/// (partial: Expand all is checked against the address before it is committed)
test("an Expand all whose open set would not fit the address is refused and the table keeps its state", async () => {
  const { call } = recorder();
  const cfg = config(standard()); // the arch groups rows by priority only: the author grouping below is a layout of the address
  const accept = (snapshot: Parameters<NonNullable<ConstructorParameters<typeof PivotModel>[5]>>[0]): void => {
    if ((snapshotParams(snapshot).pivot_open ?? "").length > 30) throw new ClientError("validation", "too large for the address");
  };
  const model = new PivotModel(cfg, call, () => [], LIMITS, { ...archState(cfg), rows: [field("priority"), field("author_id")], open: { row: [[]], col: [[]] } }, accept);
  await model.load();
  const before = labels(model.table());
  await rejects(model.expandAll(), "validation", /too large for the address/);
  expect(labels(model.table())).toEqual(before);
  expect(model.table().unavailable).toBe(false);
  await model.sortBy("size", []); // a later change still works
  expect(model.sort).not.toBeNull();
});

/// (partial: a sort whose measure is hidden survives a full read)
test("a hidden sort measure stays read by a full read, so the rows keep their order", async () => {
  const off = await made([pivotField("priority", "row", "Priority"), pivotField("size", "measure", "Size"), pivotField("weight", "measure", "Weight")], { flags: { default_order: "weight asc" } });
  await off.model.toggleMeasure("weight");
  const sorted = labels(off.model.table());
  await off.model.expandAll();
  expect(labels(off.model.table())).toEqual(sorted);
});

/// (partial: collapsing the column a sort sits under clears the sort)
test("collapsing the column header the sort sits under clears the sort and its address part", async () => {
  const { model } = await made();
  await model.sortBy("size", ["draft"].map(k));
  expect(model.snapshot().sort).not.toBeNull();
  await model.collapse("col", []);
  expect(model.sort).toBeNull();
  expect(model.snapshot().sort).toBeNull();
});

test("Collapse all clears a sort that sat under a column, and its address part", async () => {
  const { model } = await made();
  await model.sortBy("size", ["draft"].map(k));
  expect(model.snapshot().sort).not.toBeNull();
  await model.collapseAll();
  expect(model.sort).toBeNull();
  expect(model.snapshot().sort).toBeNull();
});

/// (partial: a search view item grouping a non-date field by an interval is not offered)
test("a search view item with an interval on a non-date field is not offered in the expand menu", async () => {
  const original = full.views;
  full.views = original.map(item => item.type === "search"
    ? { ...item, nodes: [{ kind: "search", children: [1, 2, 3] }, ...item.nodes.slice(1), { kind: "search_filter", name: "by_author_month", label: "Bad interval", context: groupByContext("author_id:month") }] }
    : item) as typeof full.views;
  try {
    const { root } = await startApp("/erp/action-20?view_type=pivot");
    await ready(root);
    header(root, "row", [k("normal")]).click();
    const items = [...root.querySelectorAll<HTMLElement>(".pivot-field-menu [role=menuitem]")].map(item => item.textContent);
    expect(items).toContain("By author");
    expect(items).not.toContain("Bad interval");
  } finally { full.views = original; }
});


/// (partial: the search's Group By sets the row groupings: every level expanded, header clicks forgotten, null returns to the arch; the columns stay)
test("setRows replaces the row groupings with every level expanded, drops what header clicks added, and null returns to the arch's rows", async () => {
  const { model, requests } = await made();
  await model.expand("row", [k("normal")], field("author_id"));
  expect(labels(model.table())).toContain("    Ada");
  requests.length = 0;
  await model.setRows([field("author_id"), field("published", "month")]);
  expect(requests).toHaveLength(1);
  expect(requests[0]!.groupingSets).toEqual([[], ["state"], ["author_id"], ["author_id", "state"], ["author_id", "published:month"], ["author_id", "published:month", "state"]]);
  expect(labels(model.table())).toEqual(["Total", "  Ada", "    January 2026", "    February 2026", "  Bo", "    February 2026", "    April 2026", "  None", "    None"]);
  expect(model.table().headerRows.flat().map(header => header.label)).toEqual(["Total", "Draft", "Done"]); // the columns are the search's no business
  expect(model.snapshot()).toMatchObject({ rows: ["author_id", "published:month"], cols: ["state"], layoutDefault: false });
  await model.setRows(null);
  expect(labels(model.table())).toEqual(["Total", "  Normal", "  High", "  None"]);
  expect(model.snapshot()).toMatchObject({ rows: ["priority"], layoutDefault: true });
  await rejects(model.setRows(Array.from({ length: 7 }, () => field("author_id"))), "validation", /at most 6/);
});

const nodes = (root: HTMLElement, axis: "row" | "col", path: string[]): HTMLElement => header(root, axis, path).closest("th")!;

/// (partial: every header is a node that says closed or opened, with the whole cell as its target)
test("each header shows a closed or opened node, its title is a tooltip, and a click anywhere in the cell works like the button", async () => {
  const { root, settle, model } = await mounted([pivotField("priority", "row", "Priority"), pivotField("state", "row", "State"), pivotField("size", "measure", "Size")]);
  const total = nodes(root, "row", []), leaf = nodes(root, "row", [k("normal"), k("draft")]);
  expect(total.classList.contains("pivot-node-opened")).toBe(true);
  expect(leaf.classList.contains("pivot-node-closed")).toBe(true);
  expect(header(root, "row", []).querySelector("svg")!.innerHTML).not.toBe(header(root, "row", [k("normal"), k("draft")]).querySelector("svg")!.innerHTML);
  expect(header(root, "row", [k("normal")]).title).toBe("Normal");
  const expand = vi.spyOn(model, "expand"), collapse = vi.spyOn(model, "collapse");
  header(root, "row", [k("normal")]).click(); // the button inside: the cell around it must not act a second time
  await settle();
  expect(collapse).toHaveBeenCalledTimes(1);
  expect(expand).not.toHaveBeenCalled();
  expect(nodes(root, "row", [k("normal")]).classList.contains("pivot-node-closed")).toBe(true);
  expect(header(root, "row", [k("normal")]).getAttribute("aria-expanded")).toBe("false");
  nodes(root, "row", [k("normal")]).click(); // the cell itself opens it by the next grouping
  await settle();
  expect(expand).toHaveBeenCalledTimes(1);
  expect(nodes(root, "row", [k("normal")]).classList.contains("pivot-node-opened")).toBe(true);
  nodes(root, "col", []).click();
  await settle();
  expect(root.querySelector(".pivot-field-menu")!.classList.contains("open")).toBe(true); // a folded leaf with no grouping below asks which field
});

const searchPanel = async (root: HTMLElement): Promise<HTMLElement> => {
  root.querySelector<HTMLButtonElement>(".search-menu-toggle")!.click();
  await vi.waitFor(() => expect(root.querySelectorAll(".erp-groupby-section .erp-search-menu-item").length).toBeGreaterThan(0));
  return root.querySelector<HTMLElement>(".erp-search-menu-panel")!;
};
const groupByItem = (panel: HTMLElement, text: string): HTMLButtonElement =>
  [...panel.querySelectorAll<HTMLButtonElement>(".erp-groupby-section .erp-search-menu-item")].find(item => item.textContent === text)!;

/// (the search view's Group By sets the rows of the pivot in the order chosen; removing the facet returns to the arch's rows; reference pivot_model.js)
test("the search's Group By sets the pivot's rows in the order chosen, a header click's level is forgotten, and removing the facet brings the arch's rows back", async () => {
  const { root, of } = await startApp("/erp/action-20?view_type=pivot");
  await ready(root);
  header(root, "row", [k("normal")]).click();
  choose(root, "Author");
  await vi.waitFor(() => expect(cells(root, ".pivot-row-header")).toEqual(["Total", "Normal", "Ada", "High", "None"]));
  const panel = await searchPanel(root);
  groupByItem(panel, "By author").click();
  await vi.waitFor(() => expect(cells(root, ".pivot-row-header")).toEqual(["Total", "Ada", "Bo", "None"]));
  expect(of("formatted_read_grouping_sets").at(-1)!.body["grouping_sets"]).toEqual([[], ["state"], ["author_id"], ["author_id", "state"]]);
  expect(cells(root, ".pivot-head-groups th")).toEqual(["Draft", "Done"]);
  expect(query().get("pivot_rows")).toBe("author_id");
  expect(query().get("pivot_cols")).toBe("state");
  groupByItem(panel, "By month").click();
  await vi.waitFor(() => expect(cells(root, ".pivot-row-header")).toEqual(["Total", "Ada", "January 2026", "February 2026", "Bo", "February 2026", "April 2026", "None", "None"]));
  expect(root.querySelector(".erp-facet-groupBy")!.textContent).toContain("Group by By author > By month");
  expect(query().get("pivot_rows")).toBe("author_id,published:month");
  root.querySelector<HTMLButtonElement>(".erp-facet-groupBy .erp-facet-remove")!.click();
  await vi.waitFor(() => expect(cells(root, ".pivot-row-header")).toEqual(["Total", "Normal", "High", "None"]));
  expect(pivotParams()).toEqual([]); // the arch's table again: a clean address
  expect(cells(root, ".pivot-head-groups th")).toEqual(["Draft", "Done"]);
});

/// (partial: the address keeps the table, not the facet: a reload shows the same rows, with one read and no Group By facet)
test("the rows a search group-by made are kept in the address as pivot_rows, and opening that address shows the same table without the facet", async () => {
  const first = await startApp("/erp/action-20?view_type=pivot");
  await ready(first.root);
  groupByItem(await searchPanel(first.root), "By author").click();
  await vi.waitFor(() => expect(cells(first.root, ".pivot-row-header")).toEqual(["Total", "Ada", "Bo", "None"]));
  const address = window.location.search;
  expect(query().has("search_groupby")).toBe(false);
  const text = cells(first.root, "tbody .pivot-cell");
  closeApp();
  const again = await startApp(`/erp/action-20${address}`);
  await ready(again.root);
  expect(again.of("formatted_read_grouping_sets")).toHaveLength(1);
  expect(cells(again.root, ".pivot-row-header")).toEqual(["Total", "Ada", "Bo", "None"]);
  expect(cells(again.root, "tbody .pivot-cell")).toEqual(text);
  expect(again.root.querySelector(".erp-facet-groupBy")).toBeNull();
  // A group-by chosen now replaces those rows again.
  groupByItem(await searchPanel(again.root), "By month").click();
  await vi.waitFor(() => expect(cells(again.root, ".pivot-row-header").slice(0, 3)).toEqual(["Total", "January 2026", "February 2026"]));
});

/// (partial: a model with no search view: the pivot opens, the Group By menu offers the model's fields; the server needs the request to know)
test("a model with no default search view opens its pivot, and its Group By menu offers the groupable fields as custom groups", async () => {
  noSearchView = true;
  const { root, of } = await startApp("/erp/action-20?view_type=pivot");
  await ready(root);
  expect(of("get_views").filter(call => (call.body["views"] as unknown[][]).some(item => item[1] === "search"))).toHaveLength(1);
  root.querySelector<HTMLButtonElement>(".search-menu-toggle")!.click();
  await vi.waitFor(() => expect(root.querySelector(".erp-custom-group-select")).not.toBeNull());
  expect(root.querySelectorAll(".erp-groupby-section .erp-search-menu-item")).toHaveLength(0);
  const select = root.querySelector<HTMLSelectElement>(".erp-custom-group-select")!;
  select.value = "author_id"; select.dispatchEvent(new Event("change"));
  await vi.waitFor(() => expect(cells(root, ".pivot-row-header")).toEqual(["Total", "Ada", "Bo", "None"]));
});

// ---- cycle 11 review fixes (M1, M2, S5, S7) ----

/// (partial: a list grouped from its search menu opens the pivot; the list's grouping is not carried over, the pivot keeps its arch rows)
test("a grouped list opens its Pivot view: the pivot mounts, and the address carries no list grouping", async () => {
  const { root } = await startApp("/erp/action-20");
  groupByItem(await searchPanel(root), "By author").click();
  await vi.waitFor(() => expect(root.querySelectorAll(".list-group-row").length).toBeGreaterThan(0));
  expect(query().has("search_groupby")).toBe(true);
  root.querySelector<HTMLButtonElement>('.view-btn[aria-label="pivot view"]')!.click();
  await ready(root);
  expect(query().get("view_type")).toBe("pivot");
  expect(query().has("search_groupby")).toBe(false);
  expect(cells(root, ".pivot-row-header")).toEqual(["Total", "Normal", "High", "None"]); // the arch's rows
});

/// (partial: two refusals in a row: the second search's figures are never the first search's)
test("a Group By refused twice, with a search change in between, leaves the table unavailable and never the old search's figures", async () => {
  let domain: unknown[] = [];
  const { model } = await made(standard(), { limits: { ...LIMITS, maxOpen: 2 }, domain: () => domain });
  const rows = [field("author_id"), field("published", "month")];
  await rejects(model.setRows(rows), "validation", /expanded/);
  expect(model.table().unavailable).toBe(false); // the same search: the table it had is still current
  domain = [["priority", "=", "normal"]];
  await rejects(model.setRows(rows), "validation", /expanded/);
  expect(model.table().unavailable).toBe(true);
});

/// (partial: a Group By the table cannot take is refused in its own words and the facet goes, so the chip says what the rows are)
test("a Group By refused for size names the Group By, removes its facet and keeps the rows it had", async () => {
  settings["web.pivot_max_open_headers"] = 3;
  try {
    const { root, of } = await startApp("/erp/action-20?view_type=pivot");
    await ready(root);
    const panel = await searchPanel(root);
    groupByItem(panel, "By author").click();
    await vi.waitFor(() => expect(cells(root, ".pivot-row-header")).toEqual(["Total", "Ada", "Bo", "None"]));
    groupByItem(panel, "By month").click();
    await vi.waitFor(() => expect(document.body.textContent).toContain("Group By removed:"));
    expect(document.body.textContent).toMatch(/Group By removed: The table would have more than 3 expanded headers/);
    expect(root.querySelector(".erp-facet-groupBy")).toBeNull();
    expect(cells(root, ".pivot-row-header")).toEqual(["Total", "Ada", "Bo", "None"]);
    // A filter after the refusal keeps the rows the table has: there is no Group By left to apply.
    [...document.querySelectorAll<HTMLButtonElement>("dialog button")].find(button => button.textContent === "Close")!.click();
    root.querySelector<HTMLInputElement>(".search-input")!.value = "Normal";
    const reads = of("formatted_read_grouping_sets").length;
    root.querySelector<HTMLFormElement>(".search-form")!.dispatchEvent(new Event("submit", { cancelable: true }));
    await vi.waitFor(() => expect(of("formatted_read_grouping_sets").length).toBeGreaterThan(reads));
    await vi.waitFor(() => expect(cells(root, ".pivot-row-header")).toEqual(["Total", "Ada"]));
  } finally { settings["web.pivot_max_open_headers"] = 20; }
});

/// (partial: the address bound reached by a Group By is not worded as a header problem)
test("a Group By whose table would not fit in the address says so in Group By terms and removes its facet", async () => {
  const long = (letter: string): string => letter.repeat(400);
  served = ["a", "b", "c", "d", "e", "f"].map((letter, index): Rec => ({ id: index + 1, priority: "normal", state: "draft", author: 1, size: 1, weight: 0, published: false, name: long(letter) }));
  const { root } = await startApp("/erp/action-20?view_type=pivot");
  await ready(root);
  const panel = await searchPanel(root);
  const select = panel.querySelector<HTMLSelectElement>(".erp-custom-group-select")!;
  select.value = "name"; select.dispatchEvent(new Event("change"));
  await vi.waitFor(() => expect(cells(root, ".pivot-row-header").length).toBe(7));
  groupByItem(panel, "By author").click();
  await vi.waitFor(() => expect(document.body.textContent).toContain("Group By removed:"));
  expect(document.body.textContent).toContain("Group By removed: This table is too large to keep in the page address. Choose fewer Group By fields or narrow the search.");
  expect(document.body.textContent).not.toContain("Collapse some headers");
  expect(root.querySelector(".erp-facet-groupBy")).toBeNull();
});

/// (partial: the depth of a header is named for assistive technology)
test("a pivot header button states its level and axis in an aria-description", async () => {
  const { root } = await mounted([pivotField("priority", "row", "Priority"), pivotField("state", "row", "State"), pivotField("author_id", "col", "Author"), pivotField("size", "measure", "Size")]);
  expect(header(root, "row", []).hasAttribute("aria-description")).toBe(false); // the Total has its own name
  expect(header(root, "row", [k("normal")]).getAttribute("aria-description")).toBe("Row level 1"); // the arch's rows start expanded
  expect(header(root, "row", [k("normal"), k("draft")]).getAttribute("aria-description")).toBe("Row level 2");
  expect(header(root, "col", [k(1)]).getAttribute("aria-description")).toBe("Column level 1");
});
