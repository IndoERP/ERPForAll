/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test, vi } from "vitest";
import { describeViews } from "../src/client/metadata.js";
import { readStoredMap } from "../src/client/map-view.js";

const field = (name: string) => ({ model: "res.partner", name });
const node = { kind: "map", latitude: field("lat"), longitude: field("lon") };
const metadata = { type: "float", label: "Coordinate", required: false, readonly: false, relation: null, relation_field: null, selection: null };
const read = (root = node) => describeViews({
  wire_major: 1, registry_version: "r", description_version: "d", model: "res.partner",
  fields: { "res.partner": { lat: metadata, lon: metadata } }, notices: [],
  views: [{ identity: { kind: "stored", id: 3 }, type: "map", schema_major: 1, renderer: "web.map", root: 0, nodes: [root] }]
}, "res.partner", "r").views.get("map")!;

test("one bounded record read plots visible stored coordinates, including zero and the antimeridian", async () => {
  const domain = [["map_located", "=", true], ["name", "ilike", "Port"]], context = { allowed_company_ids: [7] };
  const call = vi.fn(async () => ({ length: 4, records: [
    { id: 3, display_name: "Port A", lat: 0, lon: 0 },
    { id: 5, display_name: "Port B", lat: -10, lon: 180 },
    { id: 9, display_name: "Port C", lat: null, lon: 0 }
  ] }));
  const open = vi.fn();
  const view = await readStoredMap({ call }, "res.partner", read(), domain, context, open);
  expect(call).toHaveBeenCalledTimes(1);
  expect(call).toHaveBeenCalledWith("res.partner", "web_search_read", { domain, context, specification: { display_name: {}, lat: {}, lon: {} }, limit: 80, order: "id asc" });
  expect([...view.querySelectorAll("circle")].map(m => [m.getAttribute("cx"), m.getAttribute("cy")])).toEqual([["180","90"],["360","100"]]);
  expect(view.textContent).toContain("Coordinates unavailable");
  expect(view.textContent).toContain("Showing 3 of 4");
  view.querySelectorAll("button")[1]!.click(); expect(open).toHaveBeenCalledWith(5);
});

test("missing and duplicate coordinate field handles fail during admission", () => {
  expect(() => read({ ...node, latitude: field("private") })).toThrow();
  expect(() => read({ ...node, latitude: field("lon") })).toThrow("Invalid map coordinate fields");
});
