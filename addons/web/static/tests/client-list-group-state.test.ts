/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import { SearchModel } from "../src/client/search/search-model.js";
import { decodeGroupableFields, buildGroupByCatalogue } from "../src/client/group-by-catalogue.js";
import { decodeListState, encodePath, decodePath } from "../src/client/list-group-state.js";
import type { Field } from "../src/client/metadata.js";

const fields = () => decodeGroupableFields({ city: { string: "City", type: "char", groupable: true }, stamp: { string: "Created", type: "datetime", groupable: true } }, true);
async function search() {
  const catalogue = buildGroupByCatalogue([], () => ({}), fields(), true);
  const model = new SearchModel({ model: "res.partner", fields: new Map<string, Field>(), groupBy: { load: async () => catalogue, multiple: true, persist: true } });
  await model.loadGroupByCatalogue();
  return model;
}

test("list dimensions and temporal intervals survive URL restoration in order", async () => {
  const model = await search();
  model.addCustomGroupBy("city"); model.addCustomGroupBy("stamp:year"); model.addCustomGroupBy("stamp:month");
  const params = new URLSearchParams(); model.serializeToUrlParams(params);
  const restored = await search(); restored.loadFromUrlParams(params);
  expect(restored.getGroupBy()).toEqual(["city", "stamp:year", "stamp:month"]);
  expect(restored.getDomain()).toEqual([]);
  restored.addCustomGroupBy("city"); expect(restored.getGroupBy()).toHaveLength(3);
});

test("group paths safely round trip Unicode and slash pairs", () => {
  const path = [["city", "東京//Bandung"], ["stamp:month", "2026-10-31 17:00:00"]];
  const encoded = encodePath(path);
  expect(encoded).toMatch(/^[A-Za-z0-9_-]+$/);
  expect(decodePath(encoded)).toEqual(path);
});

test("closed list state rejects wrong versions, duplicate paths and unsafe offsets", () => {
  const base = { v: 1, by: ["city"], offset: 0, order: "name asc", open: [] };
  const path = encodePath([["city", "A"]]);
  for (const raw of [{ ...base, v: 2 }, { ...base, unknown: true }, { ...base, offset: -1 }, { ...base, open: [{ path, offset: 0 }, { path, offset: 0 }] }]) {
    expect(() => decodeListState(raw)).toThrowError(expect.objectContaining({ kind: "validation", message: "Invalid grouping state." }));
  }
});

test("invalid restored grouping does not replace the active grouping", async () => {
  const model = await search(); model.addCustomGroupBy("city");
  expect(() => model.loadFromUrlParams(new URLSearchParams({ search_groupby: '["stamp:hour"]' }))).toThrowError(expect.objectContaining({ kind: "validation", message: "Invalid search_groupby." }));
  expect(model.getGroupBy()).toEqual(["city"]);
});

test("aggregate metadata is independent of groupable field kinds", () => {
  const decoded = decodeGroupableFields({ amount: { string: "Amount", type: "float", groupable: true, aggregator: "sum" } }, true);
  expect(decoded.aggregates).toEqual([{ field: "amount", kind: "float", op: "sum" }]);
  expect(decoded.groupable.has("amount")).toBe(false);
});

/// (partial: search-model interval admission; server range handling is separate)
test("all five date intervals are admitted and explicit interval is required", async () => {
  const model = await search();
  for (const interval of ["day", "week", "month", "quarter", "year"]) model.addCustomGroupBy(`stamp:${interval}`);
  expect(model.getGroupBy()).toEqual(["stamp:day", "stamp:week", "stamp:month", "stamp:quarter", "stamp:year"]);
  expect(() => model.addCustomGroupBy("stamp")).toThrowError(expect.objectContaining({ kind: "validation", message: "Invalid group by field." }));
});

test("complete address admission rolls a rejected grouping mutation back", async () => {
  const catalogue = buildGroupByCatalogue([], () => ({}), fields(), true);
  const model = new SearchModel({ model: "res.partner", fields: new Map(), groupBy: { load: async () => catalogue, multiple: true, persist: true, accept: () => { throw new Error("address full"); } } });
  await model.loadGroupByCatalogue();
  expect(() => model.addCustomGroupBy("city")).toThrow("address full");
  expect(model.getGroupBy()).toEqual([]);
});

test("versioned state restores open paths and root paging without trusting labels or domains", async () => {
  const model = await search(), path = encodePath([["city", "Bandung"]]);
  const state = { v: 1, by: ["city"], offset: 40, order: "city desc", open: [{ path, offset: 80 }] };
  model.loadFromUrlParams(new URLSearchParams({ search_groupby: JSON.stringify(state) }));
  expect(model.getListState()).toEqual(state);
  expect(model.getDomain()).toEqual([]);
  const params = new URLSearchParams(); model.serializeToUrlParams(params);
  expect(JSON.parse(params.get("search_groupby")!)).toEqual(state);
});

test("the chosen page size survives restoration and zero is refused", () => {
  const state = { v: 1, by: ["city"], offset: 20, limit: 20, order: "city asc", open: [] };
  expect(decodeListState(state)).toEqual(state);
  expect(() => decodeListState({ ...state, limit: 0 })).toThrowError(expect.objectContaining({ kind: "validation", message: "Invalid grouping state." }));
});

test("list temporal admission preserves the pivot catalogue's bare-date default", async () => {
  const catalogue = buildGroupByCatalogue([{ kind: "search_filter", name: "by_stamp", label: "Created", domain: null, context: {} as never }], () => ({ group_by: "stamp" }), fields(), true);
  expect(catalogue.items).toEqual([{ id: "groupby_by_stamp", description: "Created", fieldName: "stamp" }]);
  const model = new SearchModel({ model: "res.partner", fields: new Map(), groupBy: { multiple: true, load: async () => catalogue } });
  await model.loadGroupByCatalogue(); model.addCustomGroupBy("stamp");
  expect(model.getGroupBy()).toEqual(["stamp"]);
});
