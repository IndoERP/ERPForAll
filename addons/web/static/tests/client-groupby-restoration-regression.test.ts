/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import { SearchModel } from "../src/client/search/search-model.js";

/// (partial: chosen dimensions survive serialization; mounted restore and open paths have separate tests)
test("a list grouping is retained in search_groupby instead of disappearing from the address", async () => {
  const options = { model: "res.partner", fields: new Map(), groupBy: { multiple: true, persist: true, load: async () => ({ items: [], fields: [{ name: "city", label: "City", kind: "char" as const, selection: null }], skipped: [] }) } };
  const model = new SearchModel(options); await model.loadGroupByCatalogue(); model.addCustomGroupBy("city");
  const params = new URLSearchParams(); model.serializeToUrlParams(params);
  expect(params.get("search_groupby")).toBe('["city"]');
});
