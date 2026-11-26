/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { button, el } from "./dom.js";
import { svg } from "./graph-view.js";
import { land } from "./map-land.js";
import { array, integer, object, type View } from "./metadata.js";
import type { RpcClient } from "./transport.js";
import { ClientError } from "./transport.js";

/** World coordinates on a local equirectangular map. The companion list also opens overlapping markers. */
export function storedMap(view: View, records: Record<string, unknown>[], open?: (id: number) => void): HTMLElement {
  const fields = view.nodes[0];
  if (fields?.kind !== "map") throw new ClientError("server", "The map has no coordinate fields.");
  const node = el("section", undefined, "stored-map");
  const chart = svg("svg", {viewBox: "0 0 360 180", role: "img", "aria-label": "Record locations"});
  chart.append(svg("path", {d: land}, "stored-map-land"));
  const list = el("ul", undefined, "stored-map-records");
  for (const row of records) {
    const id = integer(row["id"], "map record id");
    const name = typeof row["display_name"] === "string" && row["display_name"] ? row["display_name"] : `#${id}`;
    const latitude = row[fields.latitude.name], longitude = row[fields.longitude.name];
    const entry = el("li"), title = open ? button(name, () => open(id), "erp-button") : el("span", name);
    entry.append(title);
    if (typeof latitude === "number" && Number.isFinite(latitude) && Math.abs(latitude) <= 90 &&
        typeof longitude === "number" && Number.isFinite(longitude) && Math.abs(longitude) <= 180) {
      const marker = svg("circle", { cx: 180 + longitude, cy: 90 - latitude, r: 2 }, "stored-map-marker");
      const hint = svg("title", {}); hint.textContent = name; marker.append(hint);
      if (open) marker.addEventListener("click", () => open(id));
      chart.append(marker);
      entry.append(el("span", `${latitude.toFixed(4)}, ${longitude.toFixed(4)}`));
    } else entry.append(el("span", "Coordinates unavailable"));
    list.append(entry);
  }
  node.append(chart, records.length ? list : el("p", "No locations match this search."));
  return node;
}

export async function readStoredMap(rpc: Pick<RpcClient, "call">, model: string, view: View, domain: unknown[], context: Record<string, unknown>, open?: (id: number) => void): Promise<HTMLElement> {
  const fields = view.nodes[0];
  if (fields?.kind !== "map") throw new ClientError("server", "Invalid map view.");
  const limit = view.pageLimit ?? 80;
  const raw = object(await rpc.call(model, "web_search_read", {
    domain, specification: { display_name: {}, [fields.latitude.name]: {}, [fields.longitude.name]: {} }, limit, order: "id asc", context,
  }));
  const records = array(raw["records"], "map records").map(row => object(row));
  const length = integer(raw["length"], "map count");
  if (records.length > limit || length < records.length) throw new ClientError("server", "Invalid number of map records.");
  const map = storedMap(view, records, open);
  if (length > records.length) map.prepend(el("p", `Showing ${records.length} of ${length} records. Narrow the search to see the rest.`));
  return map;
}
