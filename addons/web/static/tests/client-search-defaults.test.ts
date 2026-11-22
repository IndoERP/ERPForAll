/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { describe, expect, test } from "vitest";
import { SearchModel } from "../src/client/search/search-model.js";
import { applySearchDefaults, readSearchArch, splitSearchDefaults, type SearchArch } from "../src/client/search/search-arch.js";
import { createSearchMenuPanel } from "../src/client/search/search-menu-panel.js";
import { ClientError } from "../src/client/transport.js";
import type { Field, Node } from "../src/client/metadata.js";
import type { ValidatedAst } from "../../../../web/packages/expr/src/index.js";

const field = (name: string, kind: Field["kind"], label: string, extra: Partial<Field> = {}): Field =>
  ({ name, kind, label, required: false, readonly: false, relation: null, relationField: null, selection: null, ...extra });
const fields = new Map<string, Field>([
  ["name", field("name", "char", "Name")],
  ["state", field("state", "selection", "Status", { selection: [["draft", "Draft"], ["done", "Done"]] })],
  ["approver_id", field("approver_id", "many2one", "Approver", { relation: "res.users" })],
  ["company_id", field("company_id", "many2one", "Company", { relation: "res.company" })],
]);
// The tests hand expressions in already evaluated: `evaluate` returns them unchanged and throws for a marked one.
const ast = (value: unknown): ValidatedAst => value as ValidatedAst;
const evaluate = (value: ValidatedAst): unknown => {
  if (value === (UNREADABLE as unknown)) throw new Error("name 'context_today' is not defined");
  return value;
};
const UNREADABLE = { unreadable: true };
const filterNode = (name: string, label: string, domain: unknown, context: unknown = null): Node =>
  ({ kind: "search_filter", name, label, domain: domain === null ? null : ast(domain), context: context === null ? null : ast(context) });
const separator: Node = { kind: "search_separator" };
const fieldNode = (name: string): Node => ({ kind: "search_field", field: fields.get(name)!, label: fields.get(name)!.label, filterDomain: null, operator: null });
const arch = (): Node[] => [
  fieldNode("name"),
  fieldNode("approver_id"),
  filterNode("waiting_me", "Waiting for me", [["approver_id", "=", 7]]),
  filterNode("draft", "Draft", [["state", "=", "draft"]]),
  separator,
  filterNode("done", "Done", [["state", "=", "done"]]),
  filterNode("by_company", "Company", null, { group_by: "company_id" }),
  filterNode("archived_too", "With archived", [], { active_test: false }),
  filterNode("overdue", "Overdue", UNREADABLE),
];
function model(read: SearchArch = readSearchArch(arch(), evaluate)): SearchModel {
  const search = new SearchModel({ model: "x.request", fields, groupBy: { load: async () => ({
    items: [{ id: "groupby_by_company", description: "Company", fieldName: "company_id" }],
    fields: [{ name: "company_id", label: "Company", kind: "many2one", selection: null }], skipped: [] }) } });
  search.addDeclaredFilters(read.filters, read.skipped);
  return search;
}

describe("the search view's named filters", () => {
  test("are offered in arch order; filters without a separator are OR-ed and groups AND-ed", () => {
    const read = readSearchArch(arch(), evaluate);
    expect(read.filters.map(item => [item.id, item.description])).toEqual([["view_waiting_me", "Waiting for me"], ["view_draft", "Draft"], ["view_done", "Done"]]);
    const search = model(read);
    search.toggleFilter("view_waiting_me");
    search.toggleFilter("view_draft");
    search.toggleFilter("view_done");
    expect(search.getDomain()).toEqual(["|", ["approver_id", "=", 7], ["state", "=", "draft"], ["state", "=", "done"]]);
  });

  test("a field or a group closes a filter group like a separator does", () => {
    const read = readSearchArch([filterNode("a", "A", [["name", "=", "a"]]), fieldNode("name"), filterNode("b", "B", [["name", "=", "b"]]),
      { kind: "search_group", label: "More", expand: false, children: [filterNode("c", "C", [["name", "=", "c"]])] }, filterNode("d", "D", [["name", "=", "d"]])], evaluate);
    expect(new Set(read.filters.map(item => item.groupNumber)).size).toBe(4);
  });

  test("group-by items and fields are named for defaults; a filter the menu cannot offer is reported with its reason", () => {
    const read = readSearchArch(arch(), evaluate);
    expect([...read.groupByNames]).toEqual(["by_company"]);
    expect([...read.fieldNames]).toEqual(["name", "approver_id"]);
    expect(read.skipped).toEqual([
      { description: "With archived", reason: "it sets a search context, which is not available yet" },
      { description: "Overdue", reason: "its domain cannot be read: name 'context_today' is not defined" },
    ]);
    const twice = readSearchArch([filterNode("a", "A", []), filterNode("a", "Again", []), filterNode("a.b", "Dotted", [])], evaluate);
    expect(twice.skipped).toEqual([{ description: "Again", reason: "another filter is already named a" }, { description: "Dotted", reason: "its name \"a.b\" is not a plain identifier" }]);
  });

  test("the panel lists them and names the ones it leaves out", () => {
    const search = model();
    const panel = createSearchMenuPanel(search, () => undefined);
    document.body.append(panel.element);
    panel.open();
    const labels = [...panel.element.querySelectorAll(".erp-filters-section .erp-item-label")].map(node => node.textContent);
    expect(labels).toEqual(["Waiting for me", "Draft", "Done"]);
    expect(panel.element.querySelector(".erp-filters-section .erp-search-section-status")?.textContent)
      .toBe("Not offered here: With archived (it sets a search context, which is not available yet); Overdue (its domain cannot be read: name 'context_today' is not defined).");
    panel.dispose();
  });
});

describe("search_default_<name>", () => {
  test("is taken out of the context that travels with requests", () => {
    const { context, defaults } = splitSearchDefaults({ lang: "en_US", default_state: "draft", search_default_waiting_me: 1, search_default_name: "Atlas" });
    expect(context).toEqual({ lang: "en_US", default_state: "draft" });
    expect([...defaults]).toEqual([["waiting_me", 1], ["name", "Atlas"]]);
    expect(() => splitSearchDefaults({ "search_default_a-b": 1 })).toThrowError(expect.objectContaining({ kind: "validation" }));
    expect(() => splitSearchDefaults({ search_default_a: [1] })).toThrowError(expect.objectContaining({ kind: "validation" }));
  });

  test("activates a filter the user can still remove, a group-by item and a field facet", async () => {
    const search = model();
    const unapplied = await applySearchDefaults(search, readSearchArch(arch(), evaluate),
      new Map<string, unknown>([["waiting_me", 1], ["by_company", true], ["approver_id", 7], ["name", "Atlas"], ["done", 0]]),
      async (relation, id) => `${relation} #${id} name`);
    expect(unapplied).toEqual([]);
    expect(search.getFacets().map(facet => [facet.type, facet.title, facet.values])).toEqual([
      ["filter", "Waiting for me", ["Waiting for me"]],
      ["groupBy", "Group By", ["Company"]],
      ["field", "Approver", ["7"]],
      ["field", "Name", ["Atlas"]],
    ]);
    expect(search.valueLabel("approver_id", "=", "7")).toBe("res.users #7 name");
    expect(search.getGroupBy()).toEqual(["company_id"]);
    expect(search.getDomain()).toEqual([["approver_id", "=", 7], ["name", "ilike", "Atlas"], ["approver_id", "=", 7]]);
    search.removeFacet("view_waiting_me");
    expect(search.getFilters().find(item => item.id === "view_waiting_me")?.isActive).toBe(false);
  });

  test("names what it cannot apply and keeps the rest", async () => {
    const search = model();
    const unapplied = await applySearchDefaults(search, readSearchArch(arch(), evaluate),
      new Map<string, unknown>([["missing", 1], ["draft", true], ["state", "nope"], ["archived_too", 1]]));
    expect(unapplied).toEqual([
      { name: "missing", reason: "the search view declares no filter, group-by item or field with this name" },
      { name: "state", reason: "the search view declares no filter, group-by item or field with this name" },
      { name: "archived_too", reason: "the search view declares no filter, group-by item or field with this name" },
    ]);
    expect(search.getFacets().map(facet => facet.title)).toEqual(["Draft"]);
  });

  test("a group-by default on a view that cannot be grouped is named, not dropped", async () => {
    const read = readSearchArch(arch(), evaluate);
    const search = new SearchModel({ model: "x.request", fields });
    search.addDeclaredFilters(read.filters, read.skipped);
    expect(await applySearchDefaults(search, read, new Map([["by_company", 1]]))).toEqual([{ name: "by_company", reason: "this view cannot be grouped" }]);
  });

  test("a refused field value is named with the search model's reason", async () => {
    const read = readSearchArch([fieldNode("state")], evaluate);
    const search = model(read);
    const unapplied = await applySearchDefaults(search, read, new Map([["state", "nope"]]));
    expect(unapplied).toEqual([{ name: "state", reason: "Invalid selection for Status." }]);
  });

  test("an unexpected failure is not turned into a notice", async () => {
    const read = readSearchArch([fieldNode("approver_id")], evaluate);
    await expect(applySearchDefaults(model(read), read, new Map([["approver_id", 7]]), async () => { throw new ClientError("network", "offline"); }))
      .rejects.toMatchObject({ kind: "network" });
  });

  test("an emptied search stays empty in the address, so a reload does not apply the defaults again", async () => {
    const search = model();
    search.keepEmptySearch();
    await applySearchDefaults(search, readSearchArch(arch(), evaluate), new Map([["waiting_me", 1]]));
    const params = new URLSearchParams();
    search.serializeToUrlParams(params);
    expect(params.get("search_filters")).toBe(JSON.stringify(["view_waiting_me"]));
    search.removeFacet("view_waiting_me");
    search.serializeToUrlParams(params);
    expect(params.get("search_filters")).toBe("[]");
    const reloaded = model();
    expect(reloaded.loadFromUrlParams(params)).toBe(true);
    expect(reloaded.getFacets()).toEqual([]);
    // A list without defaults keeps its address free of the marker.
    const plain = model(), empty = new URLSearchParams();
    plain.serializeToUrlParams(empty);
    expect(empty.has("search_filters")).toBe(false);
  });
});
