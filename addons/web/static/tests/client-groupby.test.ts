/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { HumanApp } from "../src/client/app.js";
import { configureClientTemplates } from "../src/client/registry.js";
import { SearchModel } from "../src/client/search/search-model.js";
import { createSearchBar } from "../src/client/search/search-bar.js";
import { encodePath } from "../src/client/list-group-state.js";
import { createGroupRow, decodeGroups, groupLabel } from "../src/client/list-groups.js";
import { buildGroupByCatalogue, decodeGroupableFields } from "../src/client/group-by-catalogue.js";
import type { Field, Node } from "../src/client/metadata.js";
import type { GroupableField, GroupByCatalogue } from "../src/client/search/types.js";
import { auditComponent } from "../../../../web/testing/axe.js";

const field = (name: string, label: string, kind: Field["kind"] = "char"): Field =>
  ({ name, kind, label, required: false, readonly: false, relation: null, relationField: null, selection: null });
const priority: GroupableField = { name: "priority", label: "Priority", kind: "selection", selection: [["normal", "Normal"], ["high", "High"]] };
const city: GroupableField = { name: "city", label: "City", kind: "char", selection: null };
const catalogue = (): GroupByCatalogue => ({
  items: [{ id: "groupby_by_priority", description: "By priority", fieldName: "priority" }],
  fields: [priority, city],
  skipped: [{ description: "By month", reason: "a date interval is not available yet" }]
});
function groupable(loader: () => Promise<GroupByCatalogue> = async () => catalogue()): SearchModel {
  const fields = new Map([["name", field("name", "Name")], ["priority", field("priority", "Priority", "selection")], ["city", field("city", "City")]]);
  return new SearchModel({ model: "t.note", fields, groupBy: { load: loader } });
}
afterEach(() => document.body.replaceChildren());

/// (partial: one active group-by from a declared item or a custom field, user choice only; defaults and the global group-by are not read)
test("a declared group-by item toggles one grouping on and off and leaves the domain alone", async () => {
  const model = groupable();
  expect(model.hasGroupBy()).toBe(true);
  expect(model.groupByReady()).toBe(false);
  expect(model.getGroupBys()).toEqual([]);
  await model.loadGroupByCatalogue();
  expect(model.getGroupBys()).toEqual([{ id: "groupby_by_priority", description: "By priority", fieldName: "priority", isActive: false }]);
  model.toggleGroupBy("groupby_by_priority");
  expect(model.getGroupBy()).toEqual(["priority"]);
  expect(model.getGroupBys()[0]!.isActive).toBe(true);
  expect(model.getFacets()).toEqual([{ id: "groupby", type: "groupBy", title: "Group By", values: ["By priority"], separator: ">", groupByField: "priority", groupById: "groupby_by_priority", groupByFields: ["priority"], groupByIds: ["groupby_by_priority"] }]);
  expect(model.getDomain()).toEqual([]);
  expect(model.getGroupableField("priority")).toEqual(priority);
  model.toggleGroupBy("groupby_by_priority");
  expect(model.getGroupBy()).toEqual([]);
  expect(model.getFacets()).toEqual([]);
});

/// (partial: a custom group-by is its own active item; same field again changes nothing)
test("a custom group-by is listed as an active item and removing its facet ungroups", async () => {
  const model = groupable();
  await model.loadGroupByCatalogue();
  model.addCustomGroupBy("city");
  expect(model.getGroupBy()).toEqual(["city"]);
  expect(model.getGroupBys().at(-1)).toEqual({ id: "custom:city", description: "City", fieldName: "city", isActive: true, custom: true });
  model.addCustomGroupBy("city");
  expect(model.getFacets()).toHaveLength(1);
  expect(model.getGroupableFields().map(f => f.name)).toEqual(["priority", "city"]);
  model.removeFacet("groupby");
  expect(model.getGroupBy()).toEqual([]);
  expect(model.getGroupBys().map(item => item.id)).toEqual(["groupby_by_priority"]);
});

/// (partial: a second simultaneous group-by is refused until nested groups exist)
test("a second grouping is refused with a clear message and changes nothing", async () => {
  const model = groupable();
  await model.loadGroupByCatalogue();
  model.toggleGroupBy("groupby_by_priority");
  const before = model.getFacets();
  expect(() => model.addCustomGroupBy("city")).toThrowError(expect.objectContaining({
    kind: "unsupported", message: "Grouping by more than one field (nested groups) is not available yet. Remove the current grouping first."
  }));
  expect(model.getFacets()).toEqual(before);
  model.clearAll();
  expect(model.getGroupBy()).toEqual([]);
});

/// (partial: unknown items and fields are validation errors; no catalogue means grouping is not offered)
test("unknown group-by items and fields fail, and a model without a catalogue refuses grouping", async () => {
  const model = groupable();
  await model.loadGroupByCatalogue();
  expect(() => model.toggleGroupBy("nope")).toThrowError(expect.objectContaining({ kind: "validation", message: "Invalid group by item." }));
  expect(() => model.addCustomGroupBy("name")).toThrowError(expect.objectContaining({ kind: "validation", message: "Invalid group by field." }));
  const plain = new SearchModel({ model: "t.note", fields: new Map() });
  expect(plain.hasGroupBy()).toBe(false);
  await expect(plain.loadGroupByCatalogue()).rejects.toMatchObject({ kind: "unsupported", message: "Grouped lists are not available yet." });
  expect(() => plain.addCustomGroupBy("city")).toThrowError(expect.objectContaining({ kind: "unsupported", message: "Grouped lists are not available yet." }));
});

/// (partial: the catalogue loads once; a failed load is returned and tried again)
test("the catalogue is loaded once, and a failure is returned to the caller and retried", async () => {
  const loader = vi.fn<() => Promise<GroupByCatalogue>>().mockRejectedValueOnce(new Error("offline")).mockImplementation(async () => catalogue());
  const model = groupable(loader);
  await expect(model.loadGroupByCatalogue()).rejects.toThrow("offline");
  expect(model.groupByReady()).toBe(false);
  await model.loadGroupByCatalogue();
  await model.loadGroupByCatalogue();
  expect(loader).toHaveBeenCalledTimes(2);
  const broken = groupable(async () => ({ ...catalogue(), items: [{ id: "groupby_x", description: "X", fieldName: "missing" }] }));
  await expect(broken.loadGroupByCatalogue()).rejects.toMatchObject({ kind: "validation", message: "Invalid group by catalogue." });
});

const fieldsReply = {
  id: { string: "ID", type: "integer", groupable: true },
  name: { string: "Name", type: "char", groupable: true },
  priority: { string: "Priority", type: "selection", groupable: true, selection: [["normal", "Normal"], ["high", "High"]] },
  notes: { string: "Notes", type: "text", groupable: true },
  stamp: { string: "Stamp", type: "datetime", groupable: true },
  lines: { string: "Lines", type: "one2many", groupable: false }
};
const filterNode = (name: string, label: string, context: unknown | null): Node =>
  ({ kind: "search_filter", name, label, domain: null, context: context as never });

/// (partial: items are read from the search view's declared filters; only the server's groupable fields in supported kinds are offered)
test("the catalogue keeps declared group-by items, offers the server's groupable fields, and lists what it leaves out with its reason", () => {
  const decoded = decodeGroupableFields(fieldsReply);
  expect([...decoded.groupable.keys()]).toEqual(["name", "priority"]);
  expect(decoded.excluded.get("notes")).toBe("grouping by text fields is not available yet");
  expect(decoded.excluded.get("lines")).toBe("the server cannot group by it");
  const spec: Record<string, unknown> = { by_priority: "priority", by_stamp: "stamp", by_month: "name:month", by_two: ["name", "priority"], by_missing: "owner_id", by_empty: "", by_text: "notes" };
  const nodes: Node[] = [
    { kind: "root", children: [
      { kind: "search_group", label: "Group By", expand: false, children: [
        ...Object.keys(spec).map(name => filterNode(name, name, { name })),
        filterNode("by_bad", "by_bad", { name: "boom" }),
        filterNode("by_priority", "Again", { name: "by_priority" })
      ] },
      filterNode("plain", "Plain", { name: "other" }),
      filterNode("no_context", "No context", null)
    ] }
  ];
  const result = buildGroupByCatalogue(nodes, context => {
    const { name } = context as unknown as { name: string };
    if (name === "other") return { default_name: "x" };
    if (name === "boom") throw new Error("bad context");
    return { group_by: spec[name] };
  }, decoded);
  expect(result.items).toEqual([{ id: "groupby_by_priority", description: "by_priority", fieldName: "priority" }]);
  expect(result.fields.map(f => f.name)).toEqual(["name", "priority"]);
  expect(result.skipped).toEqual([
    { description: "by_stamp", reason: "grouping by datetime fields is not available yet" },
    { description: "by_month", reason: "a date interval is not available yet" },
    { description: "by_two", reason: "it groups by several fields, which is not available yet" },
    { description: "by_missing", reason: "field owner_id is not readable here" },
    { description: "by_empty", reason: "its group_by is not a field name" },
    { description: "by_text", reason: "grouping by text fields is not available yet" },
    { description: "by_bad", reason: "its context cannot be read: bad context" },
    { description: "Again", reason: "another item is already named by_priority" }
  ]);
});

/// (partial: the server's field list is checked, not trusted)
test("a malformed field list is refused naming what is wrong", () => {
  expect(() => decodeGroupableFields([])).toThrowError(expect.objectContaining({ kind: "server", message: "Invalid field list." }));
  expect(() => decodeGroupableFields({ name: { string: "Name", type: "char" } })).toThrowError(expect.objectContaining({ kind: "server", message: "Invalid description of field name." }));
  expect(() => decodeGroupableFields({ priority: { string: "P", type: "selection", groupable: true, selection: [["a"]] } })).toThrowError(expect.objectContaining({ kind: "server", message: "Invalid selection of field priority." }));
});

/// (partial: the group answer of one-field grouping is decoded with a closed set of keys and checked values)
test("group answers decode to labelled groups and refuse unknown keys, duplicates and bad counts", () => {
  const rows = [
    { priority: "normal", __count: 2, __extra_domain: [["priority", "=", "normal"]] },
    { priority: false, __count: 1, __extra_domain: [["priority", "=", false]] }
  ];
  const decoded = decodeGroups({ groups: rows, length: 2 }, priority);
  expect(decoded.length).toBe(2);
  expect(decoded.groups.map(g => [g.label, g.count, g.domain])).toEqual([["Normal", 2, rows[0]!.__extra_domain], ["None", 1, rows[1]!.__extra_domain]]);
  const bad: [unknown, string][] = [
    [{ groups: [{ ...rows[0]!, extra: 1 }], length: 1 }, "Invalid group property."],
    [{ groups: [rows[0], rows[0]], length: 2 }, "Invalid group list: duplicate group."],
    [{ groups: [{ ...rows[0]!, __count: -1 }], length: 1 }, "Invalid group count."],
    [{ groups: [{ ...rows[0]!, priority: "other" }], length: 1 }, "Invalid selection value of priority."],
    [{ groups: [{ ...rows[0]!, __extra_domain: "x" }], length: 1 }, "Invalid group domain."],
    [{ groups: rows, length: 1 }, "Invalid group answer."],
    [{ groups: rows, length: 2, more: true }, "Invalid group answer."],
    [[], "Invalid group answer."]
  ];
  for (const [raw, message] of bad) expect(() => decodeGroups(raw, priority)).toThrowError(expect.objectContaining({ kind: "server", message }));
});

/// (partial: label function only; no grouped list with rows of 0 and of no value is rendered here)
test("group labels read naturally for every supported kind", () => {
  const boolean: GroupableField = { name: "done", label: "Done", kind: "boolean", selection: null };
  const owner: GroupableField = { name: "owner_id", label: "Owner", kind: "many2one", selection: null };
  const count: GroupableField = { name: "n", label: "N", kind: "integer", selection: null };
  expect([groupLabel(boolean, true), groupLabel(boolean, false)]).toEqual(["Yes", "No"]);
  expect([groupLabel(owner, [3, "Ada"]), groupLabel(owner, [4, false]), groupLabel(owner, false)]).toEqual(["Ada", "#4", "None"]);
  expect([groupLabel(count, 0), groupLabel(count, 12), groupLabel(count, false)]).toEqual(["0", "12", "None"]);
  expect([groupLabel(city, "Bandung"), groupLabel(city, ""), groupLabel(city, false)]).toEqual(["Bandung", "None", "None"]);
  expect(groupLabel(priority, false)).toBe("None");
});

/// (partial: the group answer is bounded by the page asked for, label length and domain depth)
test("a group answer larger than the page, with an oversized label or a deep domain, is refused", () => {
  const row = (value: unknown, domain: unknown[] = []) => ({ city: value, __count: 1, __extra_domain: domain });
  const two = { groups: [row("A"), row("B")], length: 2 };
  expect(() => decodeGroups(two, city, 1)).toThrowError(expect.objectContaining({ kind: "server", message: "Invalid group answer." }));
  expect(() => decodeGroups({ groups: [row("x".repeat(5000))], length: 1 }, city)).toThrowError(expect.objectContaining({ message: "Invalid group label." }));
  let deep: unknown[] = [];
  for (let level = 0; level < 12; level++) deep = [deep];
  expect(() => decodeGroups({ groups: [row("A", deep)], length: 1 }, city)).toThrowError(expect.objectContaining({ message: "Invalid group domain." }));
  expect(decodeGroups({ groups: [row("A", [["city", "=", "A"]])], length: 1 }, city).groups[0]!.count).toBe(1);
});

/// (partial: a linked record's group keeps its open/closed key when the record is renamed)
test("a linked-record group is keyed on the id, not on the name", () => {
  const owner: GroupableField = { name: "owner_id", label: "Owner", kind: "many2one", selection: null };
  const group = (name: string) => decodeGroups({ groups: [{ owner_id: [3, name], __count: 1, __extra_domain: [] }], length: 1 }, owner).groups[0]!;
  expect(group("Ada").key).toBe(group("Ada Lovelace").key);
  expect(group("Ada").label).toBe("Ada");
});

/// (partial: declared filters without a name get an id from their position)
test("two unnamed declared group-by filters both appear, under position ids", () => {
  const decoded = decodeGroupableFields(fieldsReply);
  const nodes: Node[] = [{ kind: "root", children: [filterNode("", "First", { f: "name" }), filterNode("", "Second", { f: "priority" })] }];
  const result = buildGroupByCatalogue(nodes, context => ({ group_by: (context as unknown as { f: string }).f }), decoded);
  expect(result.items).toEqual([{ id: "groupby_#0", description: "First", fieldName: "name" }, { id: "groupby_#1", description: "Second", fieldName: "priority" }]);
});

/// (partial: the group row toggle, its keyboard and its accessible name)
test("a group row is one toggle: click, Enter-style click and arrows fold and unfold, with aria-expanded and the count in its name", async () => {
  const onToggle = vi.fn();
  const head = createGroupRow({ group: { key: '"normal"', label: "Normal", count: 2, domain: [] }, columns: 3, onToggle });
  const table = document.createElement("table"); const body = document.createElement("tbody"); body.append(head.row); table.append(body); document.body.append(table);
  expect(head.row.firstElementChild!.getAttribute("colspan")).toBe("3");
  expect(head.toggle.getAttribute("aria-expanded")).toBe("false");
  expect(head.toggle.getAttribute("aria-label")).toBe("Normal, 2 records");
  expect(head.toggle.textContent).toBe("Normal(2)");
  head.toggle.click();
  expect(onToggle).toHaveBeenCalledTimes(1);
  head.toggle.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowLeft", bubbles: true, cancelable: true }));
  expect(onToggle).toHaveBeenCalledTimes(1);
  head.toggle.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowRight", bubbles: true, cancelable: true }));
  expect(onToggle).toHaveBeenCalledTimes(2);
  head.setExpanded(true);
  expect(head.isExpanded()).toBe(true);
  expect(head.toggle.getAttribute("aria-expanded")).toBe("true");
  head.toggle.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowRight", bubbles: true, cancelable: true }));
  expect(onToggle).toHaveBeenCalledTimes(2);
  head.toggle.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowLeft", bubbles: true, cancelable: true }));
  expect(onToggle).toHaveBeenCalledTimes(3);
  const one = createGroupRow({ group: { key: "1", label: "Solo", count: 1, domain: [] }, columns: 2, onToggle });
  expect(one.toggle.getAttribute("aria-label")).toBe("Solo, 1 record");
  await expect(auditComponent(table)).resolves.toBeDefined();
});

/// (partial: the Group By section of the search menu, its loading state, refusal message and facet chip)
test("the search menu lists group-by items after loading, toggles them, refuses a second grouping and shows a removable chip", async () => {
  const model = groupable();
  const changed = vi.fn();
  const bar = createSearchBar(model, changed);
  document.body.append(bar.element);
  bar.element.querySelector<HTMLButtonElement>(".search-menu-toggle")!.click();
  const panel = bar.element.querySelector<HTMLElement>(".erp-search-menu-panel")!;
  expect(panel.querySelector(".erp-groupby-section")!.textContent).toContain("Loading group-by choices");
  await vi.waitFor(() => expect(panel.querySelectorAll(".erp-groupby-section .erp-search-menu-item")).toHaveLength(1));
  const item = panel.querySelector<HTMLButtonElement>(".erp-groupby-section .erp-search-menu-item")!;
  expect(item.getAttribute("aria-checked")).toBe("false");
  expect(panel.querySelector(".erp-groupby-section")!.textContent).toContain("By month (a date interval is not available yet)");
  item.click();
  expect(changed).toHaveBeenCalledTimes(1);
  expect(panel.querySelector(".erp-groupby-section .erp-search-menu-item")!.getAttribute("aria-checked")).toBe("true");
  const chip = bar.element.querySelector<HTMLElement>(".erp-facet-groupBy")!;
  expect(chip.textContent).toContain("Group by By priority");
  expect(chip.querySelector(".erp-facet-remove")!.getAttribute("aria-label")).toBe("Remove grouping by By priority");
  const select = panel.querySelector<HTMLSelectElement>(".erp-custom-group-select")!;
  expect([...select.options].map(option => option.textContent)).toEqual(["Add Custom Group", "Priority", "City"]);
  select.value = "city"; select.dispatchEvent(new Event("change"));
  expect(panel.querySelector("[role=alert]")!.textContent).toBe("Grouping by more than one field (nested groups) is not available yet. Remove the current grouping first.");
  expect(model.getGroupBy()).toEqual(["priority"]);
  chip.querySelector<HTMLButtonElement>(".erp-facet-remove")!.click();
  expect(model.getGroupBy()).toEqual([]);
  expect(changed).toHaveBeenCalledTimes(2);
  bar.dispose();
});

/// (partial: a failed catalogue load is shown in the menu)
test("a failed group-by load is shown in the menu and the next opening tries again", async () => {
  const loader = vi.fn<() => Promise<GroupByCatalogue>>().mockRejectedValueOnce(new Error("The server is busy.")).mockImplementation(async () => catalogue());
  const bar = createSearchBar(groupable(loader), () => {});
  document.body.append(bar.element);
  const toggle = bar.element.querySelector<HTMLButtonElement>(".search-menu-toggle")!;
  toggle.click();
  const panel = bar.element.querySelector<HTMLElement>(".erp-search-menu-panel")!;
  await vi.waitFor(() => expect(panel.querySelector("[role=alert]")!.textContent).toBe("The server is busy."));
  expect(panel.querySelector(".erp-groupby-section")!.textContent).toContain("Group-by choices are unavailable.");
  toggle.click(); toggle.click();
  await vi.waitFor(() => expect(panel.querySelectorAll(".erp-groupby-section .erp-search-menu-item")).toHaveLength(1));
  bar.dispose();
});

// ---- the whole list, through the application ----

const bootstrap = {
  wire_major: 1, registry_version: "r1", asset_version: "a1",
  identity: { uid: 2, name: "Operator", internal: true },
  companies: [{ id: 1, name: "Company" }], current_company_id: 1, allowed_company_ids: [1], active_company_ids: [1],
  locale: { lang: "en_US", tz: "UTC", date_format: "%m/%d/%Y", time_format: "%H:%M:%S" },
  csrf: { logout: "test-token-not-secret" }
};
const str = (value: string) => ({ kind: "literal", value: { kind: "str", value } });
const groupByContext = (spec: string) => ({ schema: 1, nodes: [str("group_by"), str(spec), { kind: "dict", items: [[0, 1]] }] });
const modifiers = { readonly: null, required: null, invisible: null };
const listField = (name: string, label: string) => ({ kind: "field", force_save: false, field: { model: "test.note", name }, widget: name === "priority" ? "selection" : "char", options: {}, modifiers, label });
const noteDescription = {
  wire_major: 1, registry_version: "r1", description_version: "d1", model: "test.note",
  fields: { "test.note": {
    name: { type: "char", label: "Name", required: true, readonly: false, relation: null, relation_field: null, selection: null },
    priority: { type: "selection", label: "Priority", required: false, readonly: false, relation: null, relation_field: null, selection: [["normal", "Normal"], ["high", "High"]] }
  } },
  notices: [],
  views: [
    { identity: { kind: "stored", id: 1 }, type: "form", schema_major: 1, renderer: "web.form", root: 0, nodes: [{ kind: "form", record_mode: "record", children: [1] }, listField("name", "Name")] },
    { identity: { kind: "stored", id: 2 }, type: "list", schema_major: 1, renderer: "web.list", root: 0, nodes: [{ kind: "list", children: [1, 2] }, listField("name", "Name"), listField("priority", "Priority")] },
    { identity: { kind: "stored", id: 3 }, type: "search", schema_major: 1, renderer: "web.search", root: 0, nodes: [
      { kind: "search", children: [1, 2, 3, 4] },
      { kind: "search_filter", name: "by_priority", label: "By priority", context: groupByContext("priority") },
      { kind: "search_filter", name: "by_month", label: "By month", context: groupByContext("name:month") },
      { kind: "search_filter", name: "plain", label: "Plain", domain: { schema: 1, nodes: [{ kind: "literal", value: { kind: "bool", value: true } }] } },
      { kind: "search_filter", name: "by_note", label: "Notes", context: groupByContext("name") }
    ] }
  ]
};
const everyRecord = [
  { id: 1, name: "Ada", priority: "normal" }, { id: 2, name: "Bo", priority: "normal" },
  { id: 3, name: "Cy", priority: "high" }, { id: 4, name: "Di", priority: false }
];

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
afterEach(() => { app?.dispose(); app = undefined; vi.restoreAllMocks(); vi.unstubAllGlobals(); history.replaceState(null, "", "/"); });

type Call = { method: string; body: Record<string, unknown> };
const reply = (value: unknown, status = 200) => new Response(JSON.stringify(value), { status, headers: { "Content-Type": "application/json" } });
/** Start the app on the Notes list with a fixture server. `handle` may answer a call first (return undefined to use the fixture). */
async function startNotes(handle: (call: Call) => unknown | Promise<unknown> = () => undefined, options: { groups?: unknown[]; length?: number; path?: string } = {}) {
  history.replaceState(null, "", options.path ?? "/erp/action-20");
  const calls: Call[] = [];
  vi.stubGlobal("fetch", vi.fn(async (path: string, init: RequestInit) => {
    const method = path.split("/").pop()!, body = init.body ? JSON.parse(String(init.body)) as Record<string, unknown> : {};
    const call = { method, body };
    calls.push(call);
    const custom = await handle(call);
    if (custom instanceof Response) return custom;
    if (custom !== undefined) return reply(custom);
    if (method === "erp_load_web_menus") return reply({ wire_major: 1, registry_version: "r1", roots: [10], nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }] });
    if (method === "erp_load_web_action" || method === "erp_load_web_action_views") {
      const action = { wire_major: 1, id: 20, name: "Notes", model: "test.note", target: "current", views: [{ id: 2, type: "list" }, { id: 1, type: "form" }], domain: null, context: null };
      return reply(method === "erp_load_web_action_views" ? { action, views: noteDescription } : action);
    }
    if (method === "get_views") {
      // the server takes null or a positive id as a view identity and nothing else (addons/web/src/metadata.rs):
      // `false` was answered 422 on the dev/testing server (2026-11-12) while this fixture accepted it
      const refused = (body["views"] as unknown[][]).find(pair => pair[0] !== null && !(Number.isInteger(pair[0]) && (pair[0] as number) > 0));
      if (refused) return reply({ name: "erp.exceptions.ValidationError", message: `Invalid view identity ${JSON.stringify(refused[0])}.`, arguments: [], context: {} }, 422);
      return reply(noteDescription);
    }
    if (method === "fields_get") return reply(fieldsReply);
    if (method === "web_read") return reply([{ id: 1, name: "Ada", priority: "normal" }]);
    if (method === "web_read_group") {
      expect(body["groupby"]).toEqual(["priority"]);
      return reply({ groups: options.groups ?? [
        { priority: "normal", __count: 2, __extra_domain: [["priority", "=", "normal"]] },
        { priority: "high", __count: 1, __extra_domain: [["priority", "=", "high"]] },
        { priority: false, __count: 1, __extra_domain: [["priority", "=", false]] }
      ], length: options.length ?? 3 });
    }
    if (method === "web_search_read") {
      const wanted = JSON.stringify(body["domain"]);
      const records = wanted === "[]" ? everyRecord : everyRecord.filter(record => wanted.includes(`"=",${JSON.stringify(record.priority)}`));
      return reply({ length: records.length, records });
    }
    return reply([]);
  }));
  const root = document.createElement("div");
  for (const region of ["header", "navigation", "notice", "content"]) { const node = document.createElement("div"); node.setAttribute(`data-client-${region}`, ""); root.append(node); }
  document.body.append(root);
  app = new HumanApp(root, bootstrap);
  await app.start();
  const of = (method: string) => calls.filter(call => call.method === method);
  const rows = () => root.querySelectorAll(".list-table tbody tr[data-id]").length;
  const heads = () => [...root.querySelectorAll<HTMLButtonElement>(".list-group-toggle")];
  /** Group by Priority through the menu. */
  const groupByPriority = async () => {
    root.querySelector<HTMLButtonElement>(".search-menu-toggle")!.click();
    const panel = root.querySelector<HTMLElement>(".erp-search-menu-panel")!;
    await vi.waitFor(() => expect(panel.querySelectorAll(".erp-groupby-section .erp-search-menu-item").length).toBeGreaterThan(0));
    panel.querySelector<HTMLButtonElement>(".erp-groupby-section .erp-search-menu-item")!.click();
    await vi.waitFor(() => expect(root.querySelectorAll(".list-group-row").length).toBeGreaterThan(0));
    root.querySelector<HTMLButtonElement>(".search-menu-toggle")!.click();
    return panel;
  };
  const search = (text: string) => {
    const input = root.querySelector<HTMLInputElement>(".search-input")!;
    input.value = text;
    root.querySelector<HTMLFormElement>(".search-form")!.dispatchEvent(new Event("submit", { cancelable: true }));
  };
  return { root, calls, of, rows, heads, groupByPriority, search };
}
const deferred = () => { let resolve!: () => void; const promise = new Promise<void>(done => { resolve = done; }); return { promise, resolve }; };

/// (partial: the grouped list reads its groups with web_read_group; aggregates, nested groups and the address are not covered)
/// (partial: opening a group loads its first page of records, closing folds it; no per-group pager)
test("a list grouped from the search menu shows folded group rows, opens and closes them, keeps selection honest and ungroups", async () => {
  const { root, of, rows, heads } = await startNotes();

  // No grouping: exactly the list as before; the menu has not asked for group-by choices yet.
  expect(rows()).toBe(4);
  expect(root.querySelector(".list-group-row")).toBeNull();
  expect(of("web_read_group")).toHaveLength(0);
  expect(of("fields_get")).toHaveLength(0);

  // Opening the menu loads the catalogue: the declared items that work here, the others with their reason.
  root.querySelector<HTMLButtonElement>(".search-menu-toggle")!.click();
  const panel = root.querySelector<HTMLElement>(".erp-search-menu-panel")!;
  await vi.waitFor(() => expect([...panel.querySelectorAll(".erp-groupby-section .erp-search-menu-item")].map(item => item.textContent)).toEqual(["By priority", "Notes"]));
  expect(of("fields_get")[0]!.body["attributes"]).toEqual(["string", "type", "groupable", "selection", "aggregator", "currency_field"]);
  expect(panel.querySelector(".erp-groupby-section")!.textContent).toContain("By month (only date fields accept an interval)");
  expect([...panel.querySelectorAll<HTMLOptionElement>(".erp-custom-group-select option")].map(option => option.textContent)).toEqual(["Add Custom Group", "Name", "Priority", "Year", "Quarter", "Month", "Week", "Day"]);

  panel.querySelector<HTMLButtonElement>(".erp-groupby-section .erp-search-menu-item")!.click();
  await vi.waitFor(() => expect(root.querySelectorAll(".list-group-row")).toHaveLength(3));
  const grouped = of("web_read_group").at(-1)!.body;
  expect(grouped).toMatchObject({ domain: [], groupby: ["priority"], aggregates: ["__count"], offset: 0, limit: 40 });
  expect(Object.hasOwn(grouped, "order")).toBe(false);
  expect(heads().map(head => head.getAttribute("aria-label"))).toEqual(["Normal, 2 records", "High, 1 record", "None, 1 record"]);
  expect(heads().every(head => head.getAttribute("aria-expanded") === "false")).toBe(true);
  expect(rows()).toBe(0);
  expect(root.querySelector<HTMLInputElement>(".list-select-all")!.disabled).toBe(true);
  expect(root.querySelector(".erp-facet-groupBy")!.textContent).toContain("Group by By priority");
  expect(root.querySelector(".pager-count")!.textContent).toBe("1–3 / 3 groups");

  // Opening a group reads its records with the group's own domain added to the list's.
  heads()[0]!.click();
  await vi.waitFor(() => expect(rows()).toBe(2));
  expect(of("web_search_read").at(-1)!.body).toMatchObject({ domain: [["priority", "=", "normal"]], offset: 0, limit: 40, order: "id asc" });
  expect(heads()[0]!.getAttribute("aria-expanded")).toBe("true");
  expect(heads()[0]!.hasAttribute("aria-busy")).toBe(false);
  expect(root.querySelector(".list-group-row")!.nextElementSibling!.getAttribute("data-id")).toBe("1");

  // A selected record leaves the selection when its group folds.
  root.querySelector<HTMLInputElement>(".list-row-select")!.click();
  expect(root.querySelector(".selection-count")!.textContent).toBe("1 selected");
  heads()[0]!.click();
  await vi.waitFor(() => expect(rows()).toBe(0));
  expect(heads()[0]!.getAttribute("aria-expanded")).toBe("false");
  expect(root.querySelector<HTMLElement>(".selection-box")!.style.display).toBe("none");

  // The unset group has a clear label and loads the unset records; sorting keeps the open group open and sorts inside it.
  heads()[2]!.click();
  await vi.waitFor(() => expect(rows()).toBe(1));
  expect(of("web_search_read").at(-1)!.body["domain"]).toEqual([["priority", "=", false]]);
  const callsBeforeSort = of("web_search_read").length;
  root.querySelector<HTMLButtonElement>("th.sortable-column .sort-button")!.click();
  await vi.waitFor(() => expect(of("web_search_read").slice(callsBeforeSort).some(call => call.body["order"] === "name asc")).toBe(true));
  expect(Object.hasOwn(of("web_read_group").at(-1)!.body, "order")).toBe(false);
  await vi.waitFor(() => expect(rows()).toBe(1));
  expect(root.querySelector<HTMLButtonElement>('.list-group-row[data-group-key="false"] .list-group-toggle')!.getAttribute("aria-expanded")).toBe("true");
  // Sorting by the grouping field orders the groups themselves.
  [...root.querySelectorAll<HTMLButtonElement>("th.sortable-column .sort-button")].find(button => button.textContent === "Priority")!.click();
  await vi.waitFor(() => expect(of("web_read_group").at(-1)!.body["order"]).toBe("priority asc"));

  const panel2 = root.querySelector<HTMLElement>(".erp-search-menu-panel")!;
  root.querySelector<HTMLButtonElement>(".search-menu-toggle")!.click();
  // Removing the chip returns the plain list, and the open menu unchecks the item.
  root.querySelector<HTMLButtonElement>(".erp-facet-groupBy .erp-facet-remove")!.click();
  await vi.waitFor(() => expect(root.querySelector(".list-group-row")).toBeNull());
  await vi.waitFor(() => expect(rows()).toBe(4));
  expect(panel2.querySelector(".erp-groupby-section .erp-search-menu-item")!.getAttribute("aria-checked")).toBe("false");
});

/// (partial: a group asked to open twice before its records arrive reads them once and shows them once)
test("opening a group twice before its records arrive reads them once, marks it busy, and closing removes every row", async () => {
  const gate = deferred();
  let held = 0;
  const { root, of, rows, heads, groupByPriority } = await startNotes(async call => {
    if (call.method === "web_search_read" && JSON.stringify(call.body["domain"]).includes('"normal"')) { held++; await gate.promise; }
    return undefined;
  });
  await groupByPriority();
  const toggle = heads()[0]!;
  toggle.click(); toggle.click();
  await vi.waitFor(() => expect(held).toBe(1));
  expect(toggle.getAttribute("aria-busy")).toBe("true");
  gate.resolve();
  await vi.waitFor(() => expect(rows()).toBe(2));
  expect(of("web_search_read").filter(call => JSON.stringify(call.body["domain"]).includes('"normal"'))).toHaveLength(1);
  expect(held).toBe(1);
  expect(toggle.hasAttribute("aria-busy")).toBe(false);
  root.querySelector<HTMLInputElement>(".list-select-all")!.click();
  expect(root.querySelector(".selection-count")!.textContent).toBe("2 selected");
  expect(root.querySelector<HTMLInputElement>(".list-select-all")!.checked).toBe(true);
  toggle.click();
  await vi.waitFor(() => expect(toggle.getAttribute("aria-expanded")).toBe("false"));
  expect(rows()).toBe(0);
});

/// (partial: an answer for an opened group that arrives after a newer search is dropped)
test("a group's records that arrive after a newer search are not shown", async () => {
  const gate = deferred();
  const { root, of, rows, heads, groupByPriority, search } = await startNotes(async call => {
    if (call.method === "web_search_read" && JSON.stringify(call.body["domain"]) === '[["priority","=","normal"]]') await gate.promise;
    return undefined;
  });
  await groupByPriority();
  heads()[0]!.click();
  await vi.waitFor(() => expect(of("web_search_read").length).toBeGreaterThan(1));
  search("Ada");
  await vi.waitFor(() => expect(of("web_read_group")).toHaveLength(2));
  gate.resolve();
  await new Promise(done => setTimeout(done, 20));
  expect(rows()).toBe(0);
  expect(heads().every(head => head.getAttribute("aria-expanded") === "false" && !head.hasAttribute("aria-busy"))).toBe(true);
  expect(root.querySelector(".list-group-note")).toBeNull();
});

/// (partial: open groups fold on a new search; the list's domain and the group's domain are joined)
test("open groups fold on a new search, and an opened group reads the list's filter together with its own domain", async () => {
  const { of, rows, heads, groupByPriority, search } = await startNotes();
  await groupByPriority();
  heads()[0]!.click();
  await vi.waitFor(() => expect(rows()).toBe(2));
  const readsBefore = of("web_search_read").length;
  search("Ada");
  await vi.waitFor(() => expect(of("web_read_group")).toHaveLength(2));
  await vi.waitFor(() => expect(rows()).toBe(0));
  expect(heads()).toHaveLength(3);
  expect(heads().every(head => head.getAttribute("aria-expanded") === "false")).toBe(true);
  expect(of("web_search_read")).toHaveLength(readsBefore);
  expect(of("web_read_group").at(-1)!.body["domain"]).toEqual([["name", "ilike", "Ada"]]);
  heads()[0]!.click();
  await vi.waitFor(() => expect(of("web_search_read")).toHaveLength(readsBefore + 1));
  expect(of("web_search_read").at(-1)!.body["domain"]).toEqual([["name", "ilike", "Ada"], ["priority", "=", "normal"]]);
});

/// (partial: the pager moves through groups)
test("the pager of a grouped list pages the groups and counts them", async () => {
  const { root, of, groupByPriority } = await startNotes(undefined, { length: 100 });
  await groupByPriority();
  expect(root.querySelector(".pager-count")!.textContent).toBe("1–3 / 100 groups");
  root.querySelector<HTMLButtonElement>(".pager-next")!.click();
  await vi.waitFor(() => expect(of("web_read_group")).toHaveLength(2));
  expect(of("web_read_group").at(-1)!.body).toMatchObject({ offset: 40, limit: 40 });
  await vi.waitFor(() => expect(root.querySelector(".pager-count")!.textContent).toBe("41–43 / 100 groups"));
});

/// (partial: Backspace in the empty search field removes the grouping chip)
test("Backspace in the empty search field removes the grouping", async () => {
  const { root, rows, groupByPriority } = await startNotes();
  await groupByPriority();
  const input = root.querySelector<HTMLInputElement>(".search-input")!;
  input.dispatchEvent(new KeyboardEvent("keydown", { key: "Backspace", bubbles: true }));
  await vi.waitFor(() => expect(root.querySelector(".list-group-row")).toBeNull());
  expect(root.querySelector(".erp-facet-groupBy")).toBeNull();
  await vi.waitFor(() => expect(rows()).toBe(4));
});

/// (partial: groups reopened after a sort load together and a failure names its group)
test("after a sort the open groups reopen together and a group that fails is named", async () => {
  let failHigh = false;
  const { root, of, rows, heads, groupByPriority } = await startNotes(call => {
    if (failHigh && call.method === "web_search_read" && JSON.stringify(call.body["domain"]) === '[["priority","=","high"]]') return reply({ name: "x", message: "High is down" }, 400);
    return undefined;
  });
  await groupByPriority();
  heads()[0]!.click(); await vi.waitFor(() => expect(rows()).toBe(2));
  heads()[1]!.click(); await vi.waitFor(() => expect(rows()).toBe(3));
  failHigh = true;
  const before = of("web_search_read").length;
  root.querySelector<HTMLButtonElement>("th.sortable-column .sort-button")!.click();
  await vi.waitFor(() => expect(root.querySelector("[data-client-notice]")!.textContent).toContain("Could not reopen High: High is down"));
  expect(of("web_search_read").length).toBe(before + 2);
  expect(rows()).toBe(2);
  expect(heads().map(head => head.getAttribute("aria-expanded"))).toEqual(["true", "false", "false"]);
});

/// (partial: the way back from a record restores the grouping)
test("opening a record and coming back by the breadcrumb restores the grouping", async () => {
  const { root, rows, heads, of, groupByPriority } = await startNotes();
  await groupByPriority();
  heads()[0]!.click();
  await vi.waitFor(() => expect(rows()).toBe(2));
  root.querySelector<HTMLElement>(".list-table tbody tr[data-id] button.link")!.click();
  await vi.waitFor(() => expect(root.querySelector(".record-form")).not.toBeNull());
  expect(root.querySelector(".list-table")).toBeNull();
  root.querySelector<HTMLElement>(".breadcrumb-previous a, .breadcrumb-back")!.click();
  await vi.waitFor(() => expect(root.querySelectorAll(".list-group-row")).toHaveLength(3));
  expect(root.querySelector(".erp-facet-groupBy")!.textContent).toContain("Group by By priority");
  await vi.waitFor(() => expect(heads()[0]!.getAttribute("aria-expanded")).toBe("true"));
  expect(of("web_read_group").at(-1)!.body["groupby"]).toEqual(["priority"]);
});

// ----: the pivot's search groups by several fields, in the order chosen ----
const published = { name: "published", label: "Published", kind: "date", selection: null } as unknown as GroupableField;
const multi = (maxLevels = 3): SearchModel => new SearchModel({
  model: "t.note", fields: new Map([["name", field("name", "Name")]]),
  groupBy: { multiple: true, maxLevels, load: async () => ({ items: [{ id: "groupby_by_priority", description: "By priority", fieldName: "priority" }, { id: "groupby_by_month", description: "By month", fieldName: "published", interval: "month" }], fields: [priority, city, published], skipped: [] }) }
});

/// (partial: the pivot's Group By keeps several groupings in the order chosen, in one facet; an interval item carries its interval)
test("a multiple group-by facet keeps the order, removes one item, and the facet goes with the last one", async () => {
  const model = multi();
  await model.loadGroupByCatalogue();
  model.toggleGroupBy("groupby_by_month");
  model.toggleGroupBy("groupby_by_priority");
  model.addCustomGroupBy("city");
  expect(model.getGroupBy()).toEqual(["published:month", "priority", "city"]);
  expect(model.getFacets()).toHaveLength(1);
  expect(model.getFacets()[0]!.values).toEqual(["By month", "By priority", "City"]);
  expect(model.getGroupBys().map(item => [item.id, item.isActive])).toEqual([["groupby_by_priority", true], ["groupby_by_month", true], ["custom:city", true]]);
  expect(() => model.addCustomGroupBy("published:year")).toThrow(/at most 3/);
  expect(model.getGroupBy()).toEqual(["published:month", "priority", "city"]); // a refused addition changes nothing
  model.toggleGroupBy("groupby_by_priority");
  expect(model.getGroupBy()).toEqual(["published:month", "city"]);
  model.toggleGroupBy("groupby_by_month"); model.toggleGroupBy("custom:city");
  expect(model.getFacets()).toEqual([]);
});

/// (partial: a custom group on a date field names its interval)
test("a custom group-by on a date field takes the interval in its name, and an interval on another field is refused", async () => {
  const model = multi();
  await model.loadGroupByCatalogue();
  model.addCustomGroupBy("published:quarter");
  expect(model.getGroupBy()).toEqual(["published:quarter"]);
  expect(model.getFacets()[0]!.values).toEqual(["Published: Quarter"]);
  expect(() => model.addCustomGroupBy("city:month")).toThrow(/Invalid group by field/);
  const list = groupable();
  await list.loadGroupByCatalogue();
  expect(() => list.addCustomGroupBy("city:month")).toThrow(/Invalid group by field/);
});

/// (partial: the menu offers the intervals of a date field when the owner groups by several fields)
test("the search menu of a multiple group-by offers date fields by interval and stays open for the next item", async () => {
  const model = multi();
  const bar = createSearchBar(model, () => {});
  document.body.append(bar.element);
  bar.element.querySelector<HTMLButtonElement>(".search-menu-toggle")!.click();
  const panel = bar.element.querySelector<HTMLElement>(".erp-search-menu-panel")!;
  await vi.waitFor(() => expect(panel.querySelectorAll(".erp-groupby-section .erp-search-menu-item")).toHaveLength(2));
  const select = panel.querySelector<HTMLSelectElement>(".erp-custom-group-select")!;
  expect([...select.options].map(option => option.textContent)).toEqual(["Add Custom Group", "Priority", "City", "Year", "Quarter", "Month", "Week", "Day"]);
  expect([...select.querySelectorAll("optgroup")].map(group => group.label)).toEqual(["Published"]);
  select.value = "published:year"; select.dispatchEvent(new Event("change"));
  panel.querySelectorAll<HTMLButtonElement>(".erp-groupby-section .erp-search-menu-item")[0]!.click();
  expect(model.getGroupBy()).toEqual(["published:year", "priority"]);
  expect(bar.element.querySelector(".erp-facet-groupBy")!.textContent).toContain("Group by Published: Year > By priority");
  bar.dispose();
});


test("the mounted list lazily loads nested groups and keeps the state in its address", async () => {
  const { root, of, groupByPriority, heads } = await startNotes(call => {
    if (call.method === "web_read_group" && (call.body["groupby"] as string[])[0] === "name") return { groups: [{ name: "Ada", __count: 1, __extra_domain: [["name", "=", "Ada"]] }], length: 1 };
    if (call.method === "web_read_group" && (call.body["groupby"] as string[]).length === 2) return { groups: [{ priority: "normal", __count: 2, __extra_domain: [["priority", "=", "normal"]] }], length: 1 };
    return undefined;
  });
  await groupByPriority(); root.querySelector<HTMLButtonElement>(".search-menu-toggle")!.click();
  const select = root.querySelector<HTMLSelectElement>(".erp-custom-group-select")!;
  select.value = "name"; select.dispatchEvent(new Event("change"));
  await vi.waitFor(() => expect(of("web_read_group").at(-1)!.body["groupby"]).toEqual(["priority", "name"]));
  await vi.waitFor(() => expect(heads()).toHaveLength(1));
  heads()[0]!.click();
  await vi.waitFor(() => expect(heads()).toHaveLength(2));
  expect(of("web_read_group").at(-1)!.body).toMatchObject({ groupby: ["name"], domain: [["priority", "=", "normal"]], offset: 0 });
  const state = JSON.parse(new URL(location.href).searchParams.get("search_groupby")!);
  expect(state.by).toEqual(["priority", "name"]); expect(state.open).toHaveLength(1);
  heads()[1]!.click();
  await vi.waitFor(() => expect(of("web_search_read").at(-1)!.body["domain"]).toEqual([["priority", "=", "normal"], ["name", "=", "Ada"]]));
});

/// (partial: mounted leaf paging and sibling preservation)
test("paging one leaf group changes its offset and keeps a sibling open", async () => {
  const { root, of, heads, groupByPriority } = await startNotes(call => {
    if (call.method === "web_search_read" && JSON.stringify(call.body["domain"]).includes('"normal"')) return { records: [{ id: Number(call.body["offset"]) + 1, name: "Paged record", priority: "normal" }], length: 100 };
    return undefined;
  });
  await groupByPriority(); heads()[0]!.click();
  await vi.waitFor(() => expect(root.querySelector('[aria-label="Next page of Normal"]')).not.toBeNull());
  heads()[1]!.click(); await vi.waitFor(() => expect(heads()[1]!.getAttribute("aria-expanded")).toBe("true"));
  root.querySelector<HTMLButtonElement>('[aria-label="Next page of Normal"]')!.click();
  await vi.waitFor(() => expect(of("web_search_read").at(-1)!.body).toMatchObject({ offset: 40, domain: [["priority", "=", "normal"]] }));
  await vi.waitFor(() => expect(root.querySelector('.list-group-page-count')!.textContent).toBe("41–80 / 100"));
  expect(heads()[1]!.getAttribute("aria-expanded")).toBe("true");
  const state = JSON.parse(new URL(location.href).searchParams.get("search_groupby")!);
  expect(state.open.map((entry: { offset: number }) => entry.offset).sort()).toEqual([0, 40]);
});

// ---- cycle 11 review fixes (S1 to S8) ----
const notice = (root: HTMLElement): string => root.querySelector("[data-client-notice]")!.textContent!.trim();
const settle = (): Promise<void> => new Promise(done => setTimeout(done, 30));
const groupByName = async (root: HTMLElement): Promise<void> => {
  root.querySelector<HTMLButtonElement>(".search-menu-toggle")!.click();
  await vi.waitFor(() => expect(root.querySelector(".erp-custom-group-select")).not.toBeNull());
  const select = root.querySelector<HTMLSelectElement>(".erp-custom-group-select")!;
  select.value = "name"; select.dispatchEvent(new Event("change"));
  root.querySelector<HTMLButtonElement>(".search-menu-toggle")!.click();
};

/// (partial: too many open groups for the address are refused in words, and a filter does not need the open list)
test("opening more groups than the address can keep says what to do, and adding a filter still works", async () => {
  const names = Array.from({ length: 40 }, (_, i) => `Group number ${String(i).padStart(3, "0")} of the table`);
  const { root, of, heads, search } = await startNotes(call => {
    if (call.method === "web_read_group") return { groups: names.map(name => ({ name, __count: 1, __extra_domain: [["name", "=", name]] })), length: names.length };
    if (call.method === "web_search_read") return { length: 0, records: [] };
    return undefined;
  });
  await groupByName(root);
  await vi.waitFor(() => expect(heads()).toHaveLength(40));
  let opened = 0;
  for (; opened < 40; opened++) {
    heads()[opened]!.click();
    await vi.waitFor(() => expect(heads()[opened]!.getAttribute("aria-expanded") === "true" || document.querySelector("#client-error-message") !== null).toBe(true));
    if (document.querySelector("#client-error-message")) break;
  }
  expect(opened).toBeGreaterThan(10);
  expect(opened).toBeLessThan(40);
  const refusal = document.querySelector("#client-error-message")!.textContent!;
  expect(refusal).toBe("Too many groups are open to keep in the page address. Close some groups first.");
  expect(heads()[opened]!.getAttribute("aria-expanded")).toBe("false");
  // The same open list must not stop a new filter: the groups fold on a new search anyway.
  const reads = of("web_read_group").length;
  search("Ada");
  await vi.waitFor(() => expect(of("web_read_group")).toHaveLength(reads + 1));
  expect(of("web_read_group").at(-1)!.body["domain"]).toEqual([["name", "ilike", "Ada"]]);
});

/// (partial: the way back from a record keeps the record's own history entry)
test("coming back from a record to a grouped list leaves the record's history entry alone", async () => {
  const { root, rows, heads, groupByPriority } = await startNotes();
  await groupByPriority();
  heads()[0]!.click();
  await vi.waitFor(() => expect(rows()).toBe(2));
  root.querySelector<HTMLElement>(".list-table tbody tr[data-id] button.link")!.click();
  await vi.waitFor(() => expect(root.querySelector(".record-form")).not.toBeNull());
  const formAddress = location.pathname + location.search;
  const entries = history.length;
  root.querySelector<HTMLElement>(".breadcrumb-previous a, .breadcrumb-back")!.click();
  await vi.waitFor(() => expect(root.querySelectorAll(".list-group-row")).toHaveLength(3));
  await settle();
  expect(history.length).toBe(entries + 1); // the list is a new entry
  history.back();
  await vi.waitFor(() => expect(location.pathname + location.search).toBe(formAddress));
  await vi.waitFor(() => expect(root.querySelector(".record-form")).not.toBeNull());
});

/// (partial: paging the groups with a group open is not a lost group)
test("paging the root groups with a group open does not report saved groups as missing", async () => {
  const { root, heads, groupByPriority } = await startNotes(call => {
    if (call.method === "web_read_group" && call.body["offset"] !== 0) return { groups: [{ priority: "high", __count: 1, __extra_domain: [["priority", "=", "high"]] }], length: 41 };
    if (call.method === "web_read_group") return { groups: [
      { priority: "normal", __count: 2, __extra_domain: [["priority", "=", "normal"]] }, { priority: "high", __count: 1, __extra_domain: [["priority", "=", "high"]] }
    ], length: 41 };
    return undefined;
  });
  await groupByPriority();
  heads()[0]!.click();
  await vi.waitFor(() => expect(heads()[0]!.getAttribute("aria-expanded")).toBe("true"));
  root.querySelector<HTMLButtonElement>(".pager-next")!.click();
  await vi.waitFor(() => expect(root.querySelector(".pager-count")!.textContent).toBe("41–41 / 41 groups"));
  await settle();
  expect(notice(root)).not.toContain("no longer in these results");
  expect(JSON.parse(new URL(location.href).searchParams.get("search_groupby")!).open).toEqual([]);
  root.querySelector<HTMLButtonElement>(".pager-prev")!.click();
  await vi.waitFor(() => expect(root.querySelector(".pager-count")!.textContent).toBe("1–2 / 41 groups"));
  await settle();
  expect(notice(root)).not.toContain("no longer in these results");
});

/// (partial: focus after a grouped redraw stays on the control that was pressed)
test("after a group's Next page the same Next button has focus, and a user who moved on keeps their focus", async () => {
  const gate = deferred();
  let hold = false;
  const { root, heads, groupByPriority } = await startNotes(async call => {
    if (call.method === "web_search_read" && JSON.stringify(call.body["domain"]).includes('"normal"')) {
      if (hold) await gate.promise;
      return { records: [{ id: Number(call.body["offset"]) + 1, name: "Paged record", priority: "normal" }], length: 100 };
    }
    return undefined;
  });
  await groupByPriority(); heads()[0]!.click();
  await vi.waitFor(() => expect(root.querySelector('[aria-label="Next page of Normal"]')).not.toBeNull());
  root.querySelector<HTMLButtonElement>('[aria-label="Next page of Normal"]')!.focus();
  root.querySelector<HTMLButtonElement>('[aria-label="Next page of Normal"]')!.click();
  await vi.waitFor(() => expect(root.querySelector(".list-group-page-count")!.textContent).toBe("41–80 / 100"));
  expect(document.activeElement).toBe(root.querySelector('[aria-label="Next page of Normal"]'));
  // A slow answer does not take focus away from the search field.
  hold = true;
  root.querySelector<HTMLButtonElement>('[aria-label="Next page of Normal"]')!.click();
  const input = root.querySelector<HTMLInputElement>(".search-input")!;
  input.focus();
  gate.resolve();
  await vi.waitFor(() => expect(root.querySelector(".list-group-page-count")!.textContent).toBe("81–100 / 100"));
  expect(document.activeElement).toBe(input);
  // The toggle keeps its own focus after it opens a group.
  heads()[1]!.focus(); heads()[1]!.click();
  await vi.waitFor(() => expect(heads()[1]!.getAttribute("aria-expanded")).toBe("true"));
  expect(document.activeElement).toBe(heads()[1]);
});

/// (partial: a root offset from the address beyond the groups is clamped to the last page)
test("an address whose group offset is beyond the groups shows the last page, not an empty list", async () => {
  const state = JSON.stringify({ v: 1, by: ["priority"], offset: 80, limit: 40, order: "id asc", open: [] });
  const { root, of, heads } = await startNotes(call => {
    if (call.method === "web_read_group" && call.body["offset"] !== 0) return { groups: [], length: 3 };
    return undefined;
  }, { path: `/erp/action-20?search_groupby=${encodeURIComponent(state)}` });
  await vi.waitFor(() => expect(heads()).toHaveLength(3));
  expect(root.querySelector(".pager-count")!.textContent).toBe("1–3 / 3 groups");
  expect(of("web_read_group").map(call => call.body["offset"])).toEqual([80, 0]);
  expect(JSON.parse(new URL(location.href).searchParams.get("search_groupby")!).offset).toBe(0);
});

/// (partial: the nesting level is part of a group toggle's accessible name)
test("a nested group's toggle names its level", async () => {
  const flat = createGroupRow({ group: { key: '"normal"', label: "Normal", count: 2, domain: [] }, columns: 3, level: 1, onToggle: () => {} });
  const nested = createGroupRow({ group: { key: '"Ada"', label: "Ada", count: 1, domain: [] }, columns: 3, level: 2, onToggle: () => {} });
  expect(flat.toggle.getAttribute("aria-label")).toBe("Normal, 2 records");
  expect(nested.toggle.getAttribute("aria-label")).toBe("Ada, 1 record, level 2");
});

/// (partial: a declared date group-by without an interval groups by month, as the reference does: search/utils/dates.js DEFAULT_INTERVAL)
test("a declared group-by on a date field with no interval is offered with the month interval", () => {
  const decoded = decodeGroupableFields({ ...fieldsReply, published: { string: "Published", type: "date", groupable: true } }, true);
  const nodes: Node[] = [filterNode("by_date", "By date", { f: "published" }), filterNode("by_week", "By week", { f: "published:week" })];
  const result = buildGroupByCatalogue(nodes, context => ({ group_by: (context as unknown as { f: string }).f }), decoded, true, true);
  expect(result.skipped).toEqual([]);
  expect(result.items).toEqual([
    { id: "groupby_by_date", description: "By date", fieldName: "published", interval: "month" },
    { id: "groupby_by_week", description: "By week", fieldName: "published", interval: "week" }
  ]);
});

/// (partial: a reload restores the grouping and its chip; the chip removes it)
test("after a reload of a grouped list the search bar shows the Group By facet, and removing it ungroups the list", async () => {
  const state = JSON.stringify({ v: 1, by: ["priority", "name"], offset: 0, limit: 40, order: "id asc", open: [{ path: encodePath([["priority", "normal"]]), offset: 0 }] });
  const { root, heads } = await startNotes(call => {
    if (call.method === "web_read_group" && (call.body["groupby"] as string[])[0] === "name") return { groups: [{ name: "Ada", __count: 1, __extra_domain: [["name", "=", "Ada"]] }], length: 1 };
    if (call.method === "web_read_group") return { groups: [{ priority: "normal", __count: 2, __extra_domain: [["priority", "=", "normal"]] }, { priority: "high", __count: 1, __extra_domain: [["priority", "=", "high"]] }], length: 2 };
    return undefined;
  }, { path: `/erp/action-20?search_groupby=${encodeURIComponent(state)}` });
  await vi.waitFor(() => expect(heads()).toHaveLength(3)); // the open root, its child group, and the other root
  const chip = root.querySelector(".erp-facet-groupBy");
  expect(chip!.textContent).toContain("Group by");
  root.querySelector<HTMLButtonElement>(".erp-facet-groupBy .erp-facet-remove")!.click();
  await vi.waitFor(() => expect(root.querySelector(".list-group-row")).toBeNull());
  expect(new URL(location.href).searchParams.has("search_groupby")).toBe(false);
});

/// (partial: a sort that returns to page 1 does not report the open group of page 2 as missing)
test("sorting from a later page of groups does not report the saved open group as missing", async () => {
  const { root, heads, groupByPriority } = await startNotes(call => {
    if (call.method === "web_read_group") return { groups: [{ priority: call.body["offset"] === 0 ? "normal" : "high", __count: 1, __extra_domain: [["priority", "=", call.body["offset"] === 0 ? "normal" : "high"]] }], length: 41 };
    if (call.method === "web_search_read") return { length: 0, records: [] };
    return undefined;
  });
  await groupByPriority();
  root.querySelector<HTMLButtonElement>(".pager-next")!.click();
  await vi.waitFor(() => expect(root.querySelector(".pager-count")!.textContent).toBe("41–41 / 41 groups"));
  heads()[0]!.click();
  await vi.waitFor(() => expect(heads()[0]!.getAttribute("aria-expanded")).toBe("true"));
  root.querySelector<HTMLButtonElement>("th.sortable-column .sort-button")!.click();
  await vi.waitFor(() => expect(root.querySelector(".pager-count")!.textContent).toBe("1–1 / 41 groups"));
  await settle();
  expect(notice(root)).not.toContain("no longer in these results");
});
