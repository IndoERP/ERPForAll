/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The graph view: the arch, the read and the series (graph.ts), the chart and its controls (graph-view.ts), and the view in the
// application (switcher, address, search Group By, dashboard cards, the records behind a bar). The server is pivot-fixture.ts.
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { HumanApp } from "../src/client/app.js";
import { configureClientTemplates } from "../src/client/registry.js";
import { describeViews, type View } from "../src/client/metadata.js";
import { decodePivotFields } from "../src/client/pivot.js";
import { archState, GraphModel, graphParams, graphParamsState, readGraphView, type GraphConfig } from "../src/client/graph.js";
import { mountGraph, niceStep, type GraphTarget } from "../src/client/graph-view.js";
import { auditComponent } from "../../../../web/testing/axe.js";
import { answer, filterRecords, recorder, records, type Rec } from "./pivot-fixture.js";

// ---- the arch as the server delivers it (crates/erp-views wire `graph` / `graph_field`) ----

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
const fieldsReply = {
  id: { string: "ID", type: "integer", groupable: true },
  name: { string: "Name", type: "char", groupable: true },
  priority: { string: "Priority", type: "selection", groupable: true, selection: [["normal", "Normal"], ["high", "High"]] },
  state: { string: "State", type: "selection", groupable: true, selection: [["draft", "Draft"], ["done", "Done"]] },
  author_id: { string: "Author", type: "many2one", groupable: true },
  size: { string: "Size", type: "integer", groupable: true, aggregator: "sum" },
  weight: { string: "Weight", type: "float", groupable: true, aggregator: "sum" },
  published: { string: "Published", type: "date", groupable: true }
};
const ref = (name: string) => ({ model: "t.note", name });
const group = (name: string, label: string, interval?: string) => ({ kind: "graph_field", field: ref(name), label, role: "group", ...(interval ? { interval } : {}) });
const measure = (name: string, label: string) => ({ kind: "graph_field", field: ref(name), label, role: "measure" });
type Flags = { mode?: string; stacked?: boolean; cumulated?: boolean; order?: string; disable_linking?: boolean };
const graphNode = (fields: unknown[], flags: Flags = {}) => ({ kind: "graph", children: fields.map((_, index) => index + 1), mode: flags.mode ?? "bar", stacked: flags.stacked ?? true,
  cumulated: flags.cumulated ?? false, disable_linking: flags.disable_linking ?? false, ...(flags.order ? { order: flags.order } : {}) });
const graphDescription = (fields: unknown[], flags?: Flags) => ({
  wire_major: 1, registry_version: "r1", description_version: "d1", model: "t.note", fields: { "t.note": modelFields }, notices: [],
  views: [{ identity: { kind: "stored", id: 6 }, type: "graph", schema_major: 1, renderer: "web.graph", root: 0, nodes: [graphNode(fields, flags), ...fields] }]
});
const view = (fields: unknown[], flags?: Flags): View => describeViews(graphDescription(fields, flags), "t.note", "r1").views.get("graph")!;
const config = (fields: unknown[], flags?: Flags): GraphConfig => readGraphView(view(fields, flags), decodePivotFields(fieldsReply));
const standard = () => [group("priority", "Priority"), group("state", "State"), measure("size", "Size")];
const american = { decimal_point: ".", thousands_sep: ",", grouping: [3, 0] };

async function made(fields: unknown[] = standard(), options: { flags?: Flags; source?: readonly Rec[]; maxGroups?: number } = {}) {
  const { call, requests } = recorder(options.source);
  const cfg = config(fields, options.flags);
  const model = new GraphModel(cfg, call, () => [], options.maxGroups ?? 50);
  await model.load();
  return { model, requests, cfg };
}
const values = (model: GraphModel) => model.data().series.map(series => [series.label, series.points.map(point => point.value)]);

// ---- arch and field list ----

/// (partial: the client reads the delivered defaults; Count is the measure when the arch names none)
test("an arch with no flags draws bars, stacked, not cumulated, linked, measured in Count", () => {
  const cfg = config([group("priority", "Priority")]);
  expect(archState(cfg)).toMatchObject({ mode: "bar", stacked: true, cumulated: false, order: null, measure: "__count" });
  expect(cfg.disableLinking).toBe(false);
  expect(cfg.measures.map(item => item.id)).toEqual(["__count", "size", "weight"]);
});

/// (partial: the last declared measure is drawn first; the arch's labels win; a date grouping is by month unless it names an interval)
test("the last measure the arch declares is drawn first, and its labels replace the field list's", () => {
  const cfg = config([group("published", "Released"), measure("weight", "Mass"), measure("size", "Pages")], { mode: "line", order: "desc", stacked: false });
  expect(archState(cfg)).toMatchObject({ mode: "line", stacked: false, order: "desc", measure: "size" });
  expect(cfg.groupBy.map(field => [field.label, field.interval])).toEqual([["Released", "month"]]);
  expect(cfg.measures.map(item => item.label)).toEqual(["Count", "Mass", "Pages"]);
  expect(config([group("published", "Released", "year")]).groupBy[0]!.interval).toBe("year");
});

test("a delivered graph node with an unknown mode, order or role is refused by name", () => {
  expect(() => view([group("priority", "Priority")], { mode: "area" })).toThrow("Unsupported graph mode.");
  expect(() => view([group("priority", "Priority")], { order: "up" })).toThrow("Unsupported graph order.");
  expect(() => view([{ ...group("priority", "Priority"), role: "row" }])).toThrow("Unsupported graph field role.");
});

// ---- the read ----

/// (partial: one formatted_read_grouping_sets call: each grouping alone and both crossed, with the search domain)
/// (partial: the measure is summed, with the count; Count alone reads only the count)
test("one grouped read asks each grouping alone and both crossed, with the count and the measure's sum", async () => {
  const { requests, model } = await made();
  expect(requests).toEqual([{ groupingSets: [["priority"], ["state"], ["priority", "state"]], aggregates: ["__count", "size:sum"], domain: [] }]);
  await model.setMeasure("__count");
  expect(requests.at(-1)!.aggregates).toEqual(["__count"]);
  const single = await made([group("priority", "Priority")]);
  expect(single.requests[0]!.groupingSets).toEqual([["priority"]]);
  const none = await made([]);
  expect(none.requests[0]!.groupingSets).toEqual([[]]);
  expect(none.model.data().labels).toEqual(["Total"]);
  expect(none.model.data().series[0]!.points[0]!.value).toBe(5);
});

/// (partial: the axis is the first grouping in the server's order, one series per value of the second; a pair with no record draws 0 and opens nothing)
test("the first grouping is the axis and the second the series; a pair with no record is 0 and opens nothing", async () => {
  const { model } = await made();
  const data = model.data();
  expect(data.labels).toEqual(["Normal", "High", "None"]);
  expect(values(model)).toEqual([["Draft", [10, 10, 0]], ["Done", [5, 0, 1500]]]);
  expect(data.series[0]!.points[0]!.domain).toEqual([["priority", "=", "normal"], ["state", "=", "draft"]]);
  expect(data.series[1]!.points[1]!.domain).toBeUndefined();
});

test("a grouping past the Settings bound is refused with the setting named, and the chart draws as unavailable", async () => {
  const { call } = recorder();
  const model = new GraphModel(config(standard()), call, () => [], 2);
  await expect(model.load()).rejects.toThrow("Grouping by Priority gives more than 2 groups, the most one chart draws (set in Settings: Graph view, most groups). Group by another field or narrow the search.");
  expect(model.data()).toMatchObject({ unavailable: true, empty: true });
});

// ---- what changes without a read ----

/// (the chart type, stacking, cumulation and order change what is drawn without a read; a new measure reads once)
test("changing the chart type, stacking, cumulation or order reads nothing; a new measure reads once", async () => {
  const { model, requests } = await made();
  model.setLocal({ mode: "line" }); model.setLocal({ stacked: false }); model.setLocal({ cumulated: true }); model.setLocal({ order: "asc" }); model.setLocal({ mode: "pie" });
  expect(requests).toHaveLength(1);
  await model.setMeasure("weight");
  expect(requests).toHaveLength(2);
  expect(requests[1]!.aggregates).toEqual(["__count", "weight:sum"]);
});

/// (the axis sorted by each group's total, ascending or descending; no order keeps the server's)
test("an order sorts the axis by each group's total, ties kept in the server's order", async () => {
  const { model } = await made();
  model.setLocal({ order: "desc" });
  expect(model.data().labels).toEqual(["None", "Normal", "High"]);
  expect(values(model)).toEqual([["Draft", [0, 10, 10]], ["Done", [1500, 5, 0]]]);
  model.setLocal({ order: "asc" });
  expect(model.data().labels).toEqual(["High", "Normal", "None"]);
  model.setLocal({ order: null });
  expect(model.data().labels).toEqual(["Normal", "High", "None"]);
});

test("a cumulative line adds each point to the ones before it; bars are never cumulated", async () => {
  const { model } = await made([group("author_id", "Author"), measure("size", "Size")], { flags: { mode: "line" } });
  model.setLocal({ cumulated: true });
  expect(values(model)).toEqual([["Size", [15, 25]]]);
  model.setLocal({ mode: "bar" });
  expect(values(model)).toEqual([["Size", [15, 10, 1500]]]);
});

test("a line draws no point for the records with no value on its axis; bars and slices keep them", async () => {
  const { model } = await made([group("priority", "Priority"), measure("size", "Size")], { flags: { mode: "line" } });
  expect(model.data().labels).toEqual(["Normal", "High"]);
  model.setLocal({ mode: "bar" });
  expect(model.data().labels).toEqual(["Normal", "High", "None"]);
});

/// (partial: a pie keeps the values above zero, labelled by both groupings; with none, the view says so instead of a placeholder slice)
test("a pie keeps the values above zero, and a pie with none says why", async () => {
  const { model } = await made();
  model.setLocal({ mode: "pie" });
  expect(model.data().slices.map(slice => [slice.label, slice.value])).toEqual([["Normal / Draft", 10], ["Normal / Done", 5], ["High / Draft", 10], ["None / Done", 1500]]);
  const zero = records.map(rec => ({ ...rec, size: 0 }));
  const flat = await made(standard(), { source: zero, flags: { mode: "pie" } });
  expect(flat.model.data().slices).toEqual([]);
  const view = mountGraph({ model: flat.model, locale: american, run: job => { void job(); } });
  expect(view.element.querySelector(".pivot-status")!.textContent).toBe("A pie chart shows only values above zero, and this measure has none. Choose a bar or line chart.");
});

test("a search with no record draws no chart and says so", async () => {
  const { model } = await made(standard(), { source: [] });
  expect(model.data().empty).toBe(true);
  const view = mountGraph({ model, locale: american, run: job => { void job(); } });
  expect(view.element.querySelector(".graph-chart")).toBeNull();
  expect(view.element.querySelector(".pivot-status")!.textContent).toBe("There is nothing to show: no record matches the search.");
});

// ---- the address ----

/// (partial: only what differs from the arch goes to the address, and the address gives back the same state; a malformed value is refused by name)
test("the address keeps only what differs from the arch and gives back the same state", async () => {
  const { model, cfg } = await made();
  expect(graphParams(model.current, cfg)).toEqual({});
  model.setLocal({ mode: "line", cumulated: true, stacked: false, order: "desc" });
  await model.setMeasure("weight");
  await model.setGroupBy([cfg.groupable.get("author_id")!].map(field => ({ ...field, interval: null })));
  const params = graphParams(model.current, cfg);
  expect(params).toEqual({ graph_mode: "line", graph_measure: "weight", graph_groupby: "author_id", graph_order: "desc", graph_stacked: "0", graph_cumulated: "1" });
  const back = graphParamsState(params, cfg);
  expect({ ...back, groupBy: back.groupBy.map(field => field.name) }).toEqual({ ...model.current, groupBy: ["author_id"] });
  expect(graphParams(graphParamsState({ graph_order: "none" }, config(standard(), { order: "asc" })), config(standard(), { order: "asc" }))).toEqual({ graph_order: "none" });
  expect(() => graphParamsState({ graph_mode: "area" }, cfg)).toThrow("The page address has an invalid graph_mode.");
  expect(() => graphParamsState({ graph_stacked: "yes" }, cfg)).toThrow("The page address has an invalid graph_stacked.");
  expect(() => graphParamsState({ graph_groupby: "notes" }, cfg)).toThrow("The graph cannot group by notes.");
  expect(() => graphParamsState({ graph_groupby: "priority,state,author_id" }, cfg)).toThrow("The graph groups by at most 2 fields (the axis, then the series). Remove a Group By field.");
  expect(() => graphParamsState({ graph_groupby: "published,published:month" }, cfg)).toThrow("The graph is already grouped by Published. Choose another field.");
  expect(() => graphParamsState({ graph_measure: "name" }, cfg)).toThrow("The graph has no measure name.");
});

// ---- the chart ----

async function mounted(fields: unknown[] = standard(), options: { flags?: Flags; open?: (target: GraphTarget) => Promise<void> } = {}) {
  const { model, requests } = await made(fields, { flags: options.flags });
  const jobs: Promise<void>[] = [];
  const view = mountGraph({ model, locale: american, run: job => { jobs.push(job()); }, ...(options.open ? { open: options.open } : {}) });
  document.body.append(view.element);
  return { model, requests, view, root: view.element, settle: () => Promise.all(jobs) };
}
afterEach(() => document.body.replaceChildren());
const titles = (root: HTMLElement, selector: string): string[] => [...root.querySelectorAll(`${selector} > title`)].map(node => node.textContent!);
const toolbarButton = (root: HTMLElement, text: string): HTMLButtonElement => [...root.querySelectorAll<HTMLButtonElement>(".graph-toolbar button")].find(node => node.textContent === text)!;

/// (partial: stacked bars sit on each other, one colour per series; not stacked they stand side by side; no total line is drawn)
test("stacked bars sit on each other and unstacked bars stand side by side, one colour per series", async () => {
  const { root, settle } = await mounted();
  const bars = [...root.querySelectorAll<SVGRectElement>(".graph-bar")];
  expect(bars).toHaveLength(6);
  const normal = bars.filter(bar => bar.querySelector("title")!.textContent!.startsWith("Normal /"));
  expect(normal[0]!.getAttribute("x")).toBe(normal[1]!.getAttribute("x"));
  expect(Number(normal[1]!.getAttribute("y")) + Number(normal[1]!.getAttribute("height"))).toBeCloseTo(Number(normal[0]!.getAttribute("y")), 5);
  expect(new Set(bars.map(bar => bar.getAttribute("class")))).toEqual(new Set(["graph-bar graph-s1", "graph-bar graph-s2"]));
  toolbarButton(root, "Stacked").click();
  await settle();
  const apart = [...root.querySelectorAll<SVGRectElement>(".graph-bar")].filter(bar => bar.querySelector("title")!.textContent!.startsWith("Normal /"));
  expect(apart[0]!.getAttribute("x")).not.toBe(apart[1]!.getAttribute("x"));
  expect(root.querySelectorAll(".graph-line")).toHaveLength(0);
});

/// (partial: a stacked line draws each series on top of the ones before it; no area fill)
test("a stacked line draws each series on the ones before it", async () => {
  const { root, settle } = await mounted(standard(), { flags: { mode: "line" } });
  const top = (index: number): string => root.querySelectorAll(".graph-line")[index]!.getAttribute("points")!;
  const stackedTop = top(1);
  toolbarButton(root, "Stacked").click();
  await settle();
  expect(top(1)).not.toBe(stackedTop);
});

/// (partial: each bar, point and slice names its groups and its value in the locale; a slice adds its share)
test("each mark's tooltip names its groups and its value, and a slice its share", async () => {
  const { root, settle } = await mounted();
  expect(titles(root, ".graph-bar")).toContain("None / Done: 1,500");
  const pie = [...root.querySelectorAll<HTMLButtonElement>(".graph-mode")].find(node => node.dataset["mode"] === "pie")!;
  pie.click();
  await settle();
  expect(titles(root, ".graph-slice")).toEqual(["Normal / Draft: 10 (0.7%)", "Normal / Done: 5 (0.3%)", "High / Draft: 10 (0.7%)", "None / Done: 1,500 (98.4%)"]);
});

/// (a legend entry hides its series and shows it again; the chart is drawn without it)
test("a legend entry hides its series and a second click shows it again", async () => {
  const { root } = await mounted();
  const entries = [...root.querySelectorAll<HTMLButtonElement>(".graph-legend-item")];
  expect(entries.map(entry => [entry.textContent, entry.getAttribute("aria-pressed")])).toEqual([["Draft", "true"], ["Done", "true"]]);
  entries[1]!.click();
  expect(root.querySelectorAll(".graph-bar")).toHaveLength(3);
  expect(root.querySelector(".graph-legend-item:nth-child(2)")!.getAttribute("aria-pressed")).toBe("false");
  root.querySelector<HTMLButtonElement>(".graph-legend-item:nth-child(2)")!.click();
  expect(root.querySelectorAll(".graph-bar")).toHaveLength(6);
});

/// (a bar or a slice opens its records with both groups' domain; the table's figures open the same)
test("a bar, a slice and a figure of the data table open their records", async () => {
  const opened: GraphTarget[] = [];
  const { root, settle } = await mounted(standard(), { open: async target => { opened.push(target); } });
  const bar = [...root.querySelectorAll<SVGRectElement>(".graph-bar")].find(node => node.querySelector("title")!.textContent === "Normal / Draft: 10")!;
  bar.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  await settle();
  expect(opened.at(-1)).toEqual({ domain: [["priority", "=", "normal"], ["state", "=", "draft"]], labels: ["Normal", "Draft"] });
  toolbarButton(root, "Show data").click();
  root.querySelector<HTMLButtonElement>('.pivot-cell-link[aria-label="1,500 Size, None, Done: open the records"]')!.click();
  await settle();
  expect(opened.at(-1)).toEqual({ domain: [["priority", "=", false], ["state", "=", "done"]], labels: ["None", "Done"] });
  expect([...root.querySelectorAll(".graph-table tbody td")].filter(cell => !cell.querySelector("button"))).toHaveLength(2); // the two pairs with no record are no links
});

/// (a line's points and a disable_linking arch open nothing)
test("a line's points open nothing, and with disable_linking no bar opens anything", async () => {
  const opened: GraphTarget[] = [];
  const line = await mounted(standard(), { flags: { mode: "line" }, open: async target => { opened.push(target); } });
  line.root.querySelector(".graph-point")!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  await line.settle();
  expect(line.root.querySelectorAll(".graph-link")).toHaveLength(0);
  const off = await mounted(standard(), { flags: { disable_linking: true }, open: async target => { opened.push(target); } });
  off.root.querySelector(".graph-bar")!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  await off.settle();
  expect(off.root.querySelectorAll(".graph-link, .pivot-cell-link")).toHaveLength(0);
  expect(opened).toEqual([]);
});

/// (partial: a labelled toolbar, the chart types and the order as labelled pressed buttons, stacking and cumulation as switches; the audit passes on the chart and on the table)
test("the toolbar names its controls, Stacked and Cumulative are switches, and the chart and its table pass the audit", async () => {
  const { root, settle } = await mounted(standard(), { flags: { mode: "line" } });
  expect(root.querySelector(".graph-toolbar")!.getAttribute("role")).toBe("toolbar");
  expect([...root.querySelectorAll(".graph-mode")].map(node => [node.getAttribute("aria-label"), node.getAttribute("aria-pressed")])).toEqual([["Bar chart", "false"], ["Line chart", "true"], ["Pie chart", "false"]]);
  expect(toolbarButton(root, "Stacked").getAttribute("role")).toBe("switch");
  expect(toolbarButton(root, "Cumulative").getAttribute("aria-checked")).toBe("false");
  toolbarButton(root, "Cumulative").click();
  await settle();
  expect(toolbarButton(root, "Cumulative").getAttribute("aria-checked")).toBe("true");
  expect(toolbarButton(root, "Descending").getAttribute("aria-pressed")).toBe("false");
  expect(root.querySelector(".graph-chart")!.getAttribute("aria-label")).toBe("Line chart of Size. The figures are in the data table (Show data).");
  expect((await auditComponent(root)).violations.map(item => item.id)).toEqual([]);
  toolbarButton(root, "Show data").click();
  expect((await auditComponent(root)).violations.map(item => item.id)).toEqual([]);
  expect([...root.querySelectorAll(".graph-table thead th")].map(cell => cell.getAttribute("scope"))).toEqual(["col", "col", "col"]);
});

test("the pie hides Stacked, Cumulative and the order; the line alone shows Cumulative", async () => {
  const { root, settle } = await mounted();
  expect(toolbarButton(root, "Cumulative").hidden).toBe(true);
  root.querySelector<HTMLButtonElement>('.graph-mode[data-mode="pie"]')!.click();
  await settle();
  expect([toolbarButton(root, "Stacked").hidden, toolbarButton(root, "Descending").closest<HTMLElement>(".graph-group")!.hidden]).toEqual([true, true]);
});

test("grid steps are round: 1, 2 or 5 times a power of ten", () => {
  expect([0.3, 3, 7, 13, 380, 0].map(niceStep)).toEqual([0.5, 5, 10, 20, 500, 1]);
});

// The server answers `false` for a sum over records that all have no value (live: Library > Books, pages:sum).
async function mountedFalseSums(sizes: number[]) {
  const source = records.map((rec, index) => ({ ...rec, size: sizes[index]! }));
  const { call: plain } = recorder(source);
  const call: typeof plain = async request => (await plain(request) as Record<string, unknown>[][])
    .map(rows => rows.map(row => row["size:sum"] === 0 ? { ...row, "size:sum": false } : row));
  const model = new GraphModel(config(standard()), call, () => [], 50);
  await model.load();
  const view = mountGraph({ model, locale: american, run: job => { void job(); } });
  document.body.append(view.element);
  return { model, root: view.element };
}
const ticks = (root: HTMLElement): string[] => [...root.querySelectorAll(".graph-tick")].map(node => node.textContent!).filter(text => /^[0-9,.-]+$/.test(text));

test("an integer measure whose sums are all false draws zero bars on a whole-number axis", async () => {
  const { model, root } = await mountedFalseSums([0, 0, 0, 0, 0]);
  expect(values(model)).toEqual([["Draft", [0, 0, 0]], ["Done", [0, 0, 0]]]);
  expect(root.querySelectorAll(".graph-bar")).toHaveLength(6);
  expect(titles(root, ".graph-bar")).toContain("Normal / Draft: 0");
  expect(ticks(root)).toEqual(["0", "1"]);
});

test("an integer measure with false and small sums keeps whole-number ticks", async () => {
  const { model, root } = await mountedFalseSums([2, 0, 1, 0, 0]);
  expect(values(model)).toEqual([["Draft", [2, 1, 0]], ["Done", [0, 0, 0]]]);
  expect(ticks(root)).toEqual(["0", "1", "2"]);
});

// ---- through the application ----

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
const dashboardRevision = "b".repeat(64);
const full = {
  ...graphDescription(standard()),
  views: [
    { identity: { kind: "stored", id: 2 }, type: "list", schema_major: 1, renderer: "web.list", root: 0, nodes: [{ kind: "list", children: [1, 2] }, { kind: "field", force_save: false, field: ref("name"), widget: "char", options: {}, modifiers, label: "Name" },
      { kind: "dashboard", version: 1, revision: dashboardRevision, items: [{ kind: "count", name: "high", label: "High priority", domain: null, filter: "high", color: "info" }] }] },
    graphDescription(standard()).views[0]!,
    { identity: { kind: "stored", id: 3 }, type: "search", schema_major: 1, renderer: "web.search", root: 0, nodes: [
      { kind: "search", children: [1, 2, 3] },
      { kind: "search_filter", name: "by_author", label: "By author", context: groupByContext("author_id") },
      { kind: "search_filter", name: "by_month", label: "By month", context: groupByContext("published:month") },
      { kind: "search_filter", name: "by_state", label: "By state", context: groupByContext("state") }
    ] }
  ]
};
const settings: Record<string, number> = { "web.graph_max_groups": 50 };

beforeEach(() => {
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
async function startApp(path: string) {
  history.replaceState(null, "", path);
  const calls: Call[] = [];
  vi.stubGlobal("fetch", vi.fn(async (url: string, init: RequestInit) => {
    const method = url.split("/").pop()!, body = init.body ? JSON.parse(String(init.body)) as Record<string, unknown> : {};
    calls.push({ method, body });
    if (method === "erp_load_web_menus") return reply({ wire_major: 1, registry_version: "r1", roots: [10, 11], nodes: [
      { module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null },
      { module: null, id: 11, parent_id: null, label: "Other", action_id: 21, children: [], icon: null }
    ] });
    if (method === "erp_load_web_action" || method === "erp_load_web_action_views") {
      const action = { wire_major: 1, id: 20, name: "Notes", model: "t.note", target: "current", views: [{ id: 2, type: "list" }, { id: 6, type: "graph" }], domain: null, context: null };
      return reply(method === "erp_load_web_action_views" ? { action, views: full } : action);
    }
    if (method === "get_views") return reply(full);
    if (method === "erp_read_dashboard") return reply({ version: 1, hidden: false, revision: dashboardRevision, period: "week", periods: [{ id: "week", label: "Week", kind: "rolling", days: 7 }], values: [{ name: "high", value: "2", domain: [["priority", "=", "high"]] }] });
    if (method === "fields_get") return reply(fieldsReply);
    if (method === "erp_read_values") {
      // addons/base/src/ir_config_setting.rs `read_values`: a key no installed addon declares is refused like a denied one.
      const keys = body["keys"] as string[];
      if (keys.some(key => !(key in settings))) return reply({ message: "Setting is unavailable or access is denied." }, 403);
      return reply({ wire_major: 1, scope: { kind: "global" }, revision: "0", values: keys.map(key => ({ key, value: settings[key], source: "default" })) });
    }
    if (method === "formatted_read_grouping_sets") return reply(answer(body, records));
    if (method === "web_read_group") {
      const rows = answer({ domain: body["domain"], grouping_sets: [[(body["groupby"] as string[])[0]]], aggregates: ["__count"] }, records)[0]!;
      return reply({ groups: rows, length: rows.length });
    }
    if (method === "web_search_read") {
      const kept = filterRecords(body["domain"] as unknown[], records);
      return reply({ length: kept.length, records: kept.map(record => ({ id: record.id, name: record.name })) });
    }
    return reply([]);
  }));
  const root = document.createElement("div");
  for (const region of ["header", "navigation", "notice", "content"]) { const node = document.createElement("div"); node.setAttribute(`data-client-${region}`, ""); root.append(node); }
  document.body.append(root);
  app = new HumanApp(root, bootstrap);
  await app.start();
  return { root, of: (method: string) => calls.filter(call => call.method === method) };
}
const query = (): URLSearchParams => new URL(window.location.href).searchParams;
const graphParamKeys = (): string[] => [...query().keys()].filter(key => key.startsWith("graph_")).sort();
const ready = (root: HTMLElement) => vi.waitFor(() => expect(root.querySelector(".graph-chart")).not.toBeNull());
const labels = (root: HTMLElement): string[] => [...root.querySelectorAll(".graph-tick[text-anchor=middle]")].map(node => node.textContent!);
const searchPanel = async (root: HTMLElement): Promise<HTMLElement> => {
  root.querySelector<HTMLButtonElement>(".search-menu-toggle")!.click();
  await vi.waitFor(() => expect(root.querySelectorAll(".erp-groupby-section .erp-search-menu-item").length).toBeGreaterThan(0));
  return root.querySelector<HTMLElement>(".erp-search-menu-panel")!;
};
const groupByItem = (panel: HTMLElement, text: string): HTMLButtonElement =>
  [...panel.querySelectorAll<HTMLButtonElement>(".erp-groupby-section .erp-search-menu-item")].find(item => item.textContent === text)!;

/// (partial: the graph reached from the view switcher reads its bound from Settings, the field list, and the chart with one grouped call)
test("the graph is reached from the list's view switcher: its setting, the field list and one grouped call", async () => {
  const { root, of } = await startApp("/erp/action-20");
  expect([...root.querySelectorAll(".view-btn")].map(button => button.getAttribute("aria-label"))).toEqual(["list view", "graph view"]);
  root.querySelector<HTMLButtonElement>('.view-btn[aria-label="graph view"]')!.click();
  await ready(root);
  expect(query().get("view_type")).toBe("graph");
  expect(graphParamKeys()).toEqual([]);
  expect(of("erp_read_values").at(-1)!.body).toMatchObject({ scope: { kind: "global" }, keys: ["web.graph_max_groups"] });
  expect(of("formatted_read_grouping_sets")).toHaveLength(1);
  expect(of("formatted_read_grouping_sets")[0]!.body).toMatchObject({ domain: [], grouping_sets: [["priority"], ["state"], ["priority", "state"]], aggregates: ["__count", "size:sum"] });
  expect(labels(root)).toEqual(["Normal", "High", "None"]);
  expect(root.querySelector('.view-btn[aria-label="graph view"]')!.getAttribute("aria-current")).toBe("page");
  root.querySelector<HTMLButtonElement>('.view-btn[aria-label="list view"]')!.click();
  await vi.waitFor(() => expect(root.querySelector(".list-table")).not.toBeNull());
});

/// (the chart type, order and switches are kept in the address, and opening it again draws the same chart with one read)
test("the chart's state is kept in the address and a reload draws the same chart with one grouped read", async () => {
  const first = await startApp("/erp/action-20?view_type=graph");
  await ready(first.root);
  first.root.querySelector<HTMLButtonElement>('.graph-mode[data-mode="line"]')!.click();
  await vi.waitFor(() => expect(query().get("graph_mode")).toBe("line"));
  [...first.root.querySelectorAll<HTMLButtonElement>(".graph-toolbar button")].find(node => node.textContent === "Descending")!.click();
  await vi.waitFor(() => expect(query().get("graph_order")).toBe("desc"));
  expect(graphParamKeys()).toEqual(["graph_mode", "graph_order"]);
  const address = window.location.search;
  const drawn = labels(first.root);
  expect(drawn).toEqual(["Normal", "High"]); // a line has no point for the records without a priority
  closeApp();
  const again = await startApp(`/erp/action-20${address}`);
  await ready(again.root);
  expect(again.of("formatted_read_grouping_sets")).toHaveLength(1);
  expect(labels(again.root)).toEqual(drawn);
  expect(again.root.querySelector(".graph-line")).not.toBeNull();
  expect(window.location.search).toBe(address);
});

/// (the search's Group By sets the axis and the series; a third is refused by the search; no Group By brings the arch's back)
test("the search's Group By sets the axis then the series, a third grouping is refused, and removing it brings the arch's groupings back", async () => {
  const { root, of } = await startApp("/erp/action-20?view_type=graph");
  await ready(root);
  const panel = await searchPanel(root);
  groupByItem(panel, "By author").click();
  await vi.waitFor(() => expect(labels(root)).toEqual(["Ada", "Bo", "None"]));
  expect(of("formatted_read_grouping_sets").at(-1)!.body["grouping_sets"]).toEqual([["author_id"]]);
  expect(query().get("graph_groupby")).toBe("author_id");
  groupByItem(panel, "By state").click();
  await vi.waitFor(() => expect(of("formatted_read_grouping_sets").at(-1)!.body["grouping_sets"]).toEqual([["author_id"], ["state"], ["author_id", "state"]]));
  expect([...root.querySelectorAll(".graph-legend-item")].map(node => node.textContent)).toEqual(["Draft", "Done"]);
  const reads = of("formatted_read_grouping_sets").length;
  groupByItem(panel, "By month").click();
  await vi.waitFor(() => expect(root.textContent).toContain("Invalid group by: at most 2 fields."));
  expect(of("formatted_read_grouping_sets")).toHaveLength(reads);
  expect(root.querySelector(".erp-facet-groupBy")!.textContent).toContain("By author > By state");
  root.querySelector<HTMLButtonElement>(".erp-facet-groupBy .erp-facet-remove")!.click();
  await vi.waitFor(() => expect(labels(root)).toEqual(["Normal", "High", "None"]));
  expect(query().has("graph_groupby")).toBe(false);
});

/// (partial: a list dashboard card kept in the address narrows the chart the way it narrows the list)
test("a dashboard card chosen on the list stays applied when the graph opens", async () => {
  const { root, of } = await startApp(`/erp/action-20?view_type=graph&search_filters=${encodeURIComponent('["dashboard_high"]')}`);
  await ready(root);
  expect(root.textContent).not.toContain("Invalid search_filters");
  expect(of("formatted_read_grouping_sets").at(-1)!.body["domain"]).toEqual([["priority", "=", "high"]]);
  expect(labels(root)).toEqual(["High"]);
  expect(query().get("search_filters")).toBe('["dashboard_high"]');
});

/// (a bar opens the list of its records; the breadcrumb back restores the chart as it was left)
test("a bar opens its records as a list and the way back restores the chart", async () => {
  const { root, of } = await startApp("/erp/action-20?view_type=graph&graph_mode=bar&graph_stacked=0");
  await ready(root);
  const chartAddress = window.location.href;
  const bar = [...root.querySelectorAll<SVGRectElement>(".graph-bar")].find(node => node.querySelector("title")!.textContent === "Normal / Draft: 10")!;
  bar.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  await vi.waitFor(() => expect(root.querySelector(".list-table")).not.toBeNull());
  expect(query().get("method_action")).not.toBeNull();
  expect(graphParamKeys()).toEqual([]);
  expect(of("web_search_read").at(-1)!.body["domain"]).toEqual([["priority", "=", "normal"], ["state", "=", "draft"]]);
  expect(root.querySelector(".list-table tbody")!.textContent).toContain("Alpha");
  expect(root.querySelector("h1.breadcrumb-current")!.textContent).toBe("Normal / Draft");
  // The chart is the page of the menu "Notes": the path names it once, with the crumb that restores it.
  expect([...root.querySelectorAll(".form-breadcrumb-path button.link")].map(link => link.textContent)).toEqual(["Notes"]);
  root.querySelector<HTMLButtonElement>(".breadcrumb-origin")!.click();
  await ready(root);
  expect(window.location.href).toBe(chartAddress);
});

/// (partial: a related list opened under another menu keeps that menu in its breadcrumb trail)
test("a bar opens its records as a list under another menu and keeps that menu link", async () => {
  const { root } = await startApp("/erp/action-20?view_type=graph&graph_mode=bar&graph_stacked=0&menu=11");
  await ready(root);
  const bar = [...root.querySelectorAll<SVGRectElement>(".graph-bar")].find(node => node.querySelector("title")!.textContent === "Normal / Draft: 10")!;
  bar.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  await vi.waitFor(() => expect(root.querySelector(".list-table")).not.toBeNull());
  expect([...root.querySelectorAll(".form-breadcrumb-path button.link")].map(link => link.textContent)).toEqual(["Other", "Notes"]);
});

test("export uses the displayed series and measure without another grouped read", async () => {
  const { model, requests } = await made();
  const download = vi.fn();
  const view = mountGraph({ model, locale: american, run: job => { void job(); }, download });
  view.element.querySelector<HTMLButtonElement>(".graph-download")!.click();
  expect(requests).toHaveLength(1);
  const file = download.mock.calls[0]![0] as Blob;
  expect(file.type).toBe("image/svg+xml");
  const reader = new FileReader();
  const svg = new Promise<string>(resolve => { reader.onload = () => resolve(String(reader.result)); });
  reader.readAsText(file);
  expect(await svg).toContain('class="graph-chart"');
  view.dispose();
});
