/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test, vi } from "vitest";
import { ListGroupModel } from "../src/client/list-group-model.js";
import type { GroupableField } from "../src/client/search/types.js";
const city: GroupableField = { name: "city", label: "City", kind: "char", selection: null };
const answer = (value: string, total = 3) => ({ groups: [{ city: value, __count: total, __extra_domain: [["city", "=", value]] }], length: 1 });
const deferred = () => { let resolve!: (value: unknown) => void; const promise = new Promise<unknown>(r => { resolve = r; }); return { promise, resolve }; };

/// (partial: controller drill-down domains and path identity)
test("nested children have ancestor identities and independent paging domains", async () => {
  const groups = vi.fn(async () => answer("same")), records = vi.fn(async () => ({ records: [{ id: 1 }], length: 3 }));
  const model = new ListGroupModel({ dimensions: [{ spec: "city", field: city }, { spec: "city", field: city }], aggregates: [], limit: 1, groups, records });
  model.adopt(answer("A")); const parent = model.roots[0]!;
  await model.open(parent); const child = parent.children[0]!;
  await model.open(child); await model.page(child, 1);
  expect(records.mock.calls.at(-1)).toEqual([[...parent.group.domain, ...child.group.domain], 1]);
  expect(child.path).not.toBe(parent.path);
  expect(parent.offset).toBe(0); expect(child.offset).toBe(1);
});

test("collapsing a pending group invalidates its answer", async () => {
  const pending = deferred();
  const model = new ListGroupModel({ dimensions: [{ spec: "city", field: city }], aggregates: [], limit: 1, groups: async () => answer("A"), records: () => pending.promise });
  model.adopt(answer("A")); const node = model.roots[0]!;
  const opening = model.open(node); model.close(node); pending.resolve({ records: [{ id: 1 }], length: 1 }); await opening;
  expect(node.expanded).toBe(false); expect(node.records).toEqual([]);
});

test("failed paging retains the admitted page and reports the failure", async () => {
  const records = vi.fn().mockResolvedValueOnce({ records: [{ id: 1 }], length: 3 }).mockRejectedValueOnce(new Error("connection lost"));
  const model = new ListGroupModel({ dimensions: [{ spec: "city", field: city }], aggregates: [], limit: 1, groups: async () => answer("A"), records });
  model.adopt(answer("A")); const node = model.roots[0]!;
  await model.open(node); await expect(model.page(node, 1)).rejects.toThrow("connection lost");
  expect(node.offset).toBe(0); expect(node.error).toBe("connection lost"); expect(node.records).toEqual([]);
});

test("temporal buckets and requested numeric aggregates decode without rebuilding ranges", () => {
  const stamp: GroupableField = { name: "stamp", label: "Stamp", kind: "datetime", selection: null };
  const model = new ListGroupModel({ dimensions: [{ spec: "stamp:month", field: stamp }], aggregates: [{ field: "amount", kind: "float", op: "sum" }], limit: 1, groups: async () => ({}), records: async () => ({}) });
  const domain = ["&", ["stamp", ">=", "2026-10-31 17:00:00"], ["stamp", "<", "2026-11-30 17:00:00"]];
  model.adopt({ groups: [{ "stamp:month": ["2026-10-31 17:00:00", "November 2026"], "amount:sum": 0, __count: 2, __extra_domain: domain }], length: 1 });
  expect(model.roots[0]!.group.label).toBe("November 2026");
  expect(model.roots[0]!.group.domain).toEqual(domain);
  expect(model.roots[0]!.group.aggregates).toEqual({ "amount:sum": 0 });
});

test("disposal drops pending records without publishing them", async () => {
  const gate = deferred();
  const model = new ListGroupModel({ dimensions: [{ spec: "city", field: city }], aggregates: [], limit: 1, groups: async () => ({}), records: () => gate.promise });
  model.adopt(answer("A")); const node = model.roots[0]!; const opening = model.open(node); model.dispose();
  gate.resolve({ records: [{ id: 1 }], length: 1 }); await opening;
  expect(node.records).toEqual([]); expect(node.expanded).toBe(false);
});

test("duplicate expansion reads once and shortened pages reconcile once", async () => {
  const gate = deferred(), records = vi.fn().mockImplementationOnce(() => gate.promise).mockResolvedValueOnce({ records: [], length: 1 }).mockResolvedValueOnce({ records: [{ id: 1 }], length: 1 });
  const model = new ListGroupModel({ dimensions: [{ spec: "city", field: city }], aggregates: [], limit: 1, groups: async () => ({}), records });
  model.adopt(answer("A")); const node = model.roots[0]!;
  const opening = model.open(node); await model.open(node); expect(records).toHaveBeenCalledTimes(1);
  gate.resolve({ records: [{ id: 1 }], length: 3 }); await opening;
  await model.page(node, 2); expect(records.mock.calls.map(call => call[1])).toEqual([0, 2, 0]); expect(node.offset).toBe(0);
});

test("unsafe aggregate values are refused with their kind and name", () => {
  const model = new ListGroupModel({ dimensions: [{ spec: "city", field: city }], aggregates: [{ field: "quantity", kind: "integer", op: "sum" }], limit: 1, groups: async () => ({}), records: async () => ({}) });
  for (const value of [undefined, NaN, Infinity, 9007199254740992]) {
    expect(() => model.adopt({ groups: [{ city: "A", __count: 1, __extra_domain: [], "quantity:sum": value }], length: 1 })).toThrowError(expect.objectContaining({ kind: "server", message: "Invalid aggregate quantity:sum." }));
  }
});

/// (partial: intermediate paging protocol; mounted leaf paging is separate)
test("intermediate paging counts children and preserves its sibling branch", async () => {
  const groups = vi.fn(async (_domain: unknown[], _specs: string[], offset: number) => ({ groups: [{ city: String(offset), __count: 1, __extra_domain: [["city", "=", String(offset)]] }], length: 3 }));
  const model = new ListGroupModel({ dimensions: [{ spec: "city", field: city }, { spec: "city", field: city }], aggregates: [], limit: 1, groups, records: async () => ({ records: [], length: 0 }) });
  model.adopt(answer("A")); const parent = model.roots[0]!;
  await model.open(parent); await model.page(parent, 1);
  expect(groups.mock.calls.at(-1)).toEqual([parent.group.domain, ["city"], 1]);
  expect(parent.total).toBe(3); expect(parent.children[0]!.group.label).toBe("1");
});

test("an absent currency remains unknown data instead of a malformed group", () => {
  const model = new ListGroupModel({ dimensions: [{ spec: "city", field: city }], aggregates: [{ field: "amount", kind: "monetary", op: "sum", currency: "currency_id" }], limit: 1, groups: async () => ({}), records: async () => ({}) });
  model.adopt({ groups: [{ city: "A", __count: 1, __extra_domain: [], "amount:sum": 10, "currency_id:array_agg_distinct": [false] }], length: 1 });
  expect(model.roots[0]!.group.aggregates?.["currency_id:array_agg_distinct"]).toEqual([false]);
});
