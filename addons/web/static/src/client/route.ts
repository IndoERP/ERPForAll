/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { ClientError } from "./transport.js";
export interface Route {
  action?: string;
  /** Presentation-only detailed button tooltips; never an authorization flag. */
  debug?: "1";
  menu?: string;
  model?: string;
  id?: string;
  view_type?: "list" | "form" | "pivot";
  active_id?: string;
  cids?: string[];
  search?: string;
  search_facets?: string;
  search_filters?: string;
  search_groupby?: string;
  /** Page-lifetime method-action identity; its payload never travels in the URL. */
  method_action?: string;
  list_tab?: string;
  /** Pivot state: groupings by axis, measures, expanded headers, sorted column. */
  pivot_rows?: string;
  pivot_cols?: string;
  pivot_measures?: string;
  pivot_open?: string;
  pivot_sort?: string;
}
const fail = (key: string): never => {
  // A caller may supply an unknown query/property name or decimalId label.
  // Only closed contract labels may enter a displayed diagnostic.
  const label = diagnosticKeys.has(key) ? key : "url";
  throw new ClientError("validation", `Invalid navigation parameter: ${label}.`);
};
export function decimalId(value: string, key = "id"): string {
  if (typeof value !== "string" || !/^[1-9][0-9]{0,18}$/.test(value) || BigInt(value) > 9223372036854775807n) return fail(key);
  return value;
}
export function rpcId(value: string): number {
  decimalId(value);
  const id = Number(value);
  if (!Number.isSafeInteger(id)) throw new ClientError("unsupported", "This record identifier exceeds the browser's safe integer range.");
  return id;
}
const queryKeys = ["debug", "menu", "model", "id", "view_type", "active_id", "search", "search_facets", "search_filters", "search_groupby", "method_action", "list_tab", "pivot_rows", "pivot_cols", "pivot_measures", "pivot_open", "pivot_sort"] as const;
const routeKeys = new Set<string>(["action", ...queryKeys, "cids"]);
const diagnosticKeys = new Set<string>([...routeKeys, "url", "path", "hash"]);
const byteLength = (value: string): number => new TextEncoder().encode(value).length;

/** Reject lossy encoding and transport hazards before URLSearchParams can replace invalid UTF-8. */
function decode(value: string, key: string): string {
  try { return decodeURIComponent(value); }
  catch { return fail(key); }
}
function transportString(value: unknown, key: string): asserts value is string {
  if (typeof value !== "string" || !value.length || value.length > 2048
      || /[\u0000-\u001f\u007f-\u009f\\]/u.test(value) || value.includes("//")) return fail(key);
  // encodeURIComponent refuses unpaired surrogates instead of replacing them with U+FFFD.
  try { encodeURIComponent(value); } catch { return fail(key); }
}
// The pivot parameters have closed, bounded shapes (the server admits the same: crates/erp-server/src/redirect.rs).
const pivotField = (value: string): boolean => value.length <= 128 && /^[a-z_][a-z0-9_]*$/.test(value);
function pivotSpecs(value: string, measures: boolean): boolean {
  const parts = value.split(",");
  return parts.length <= (measures ? 16 : 6) && new Set(parts).size === parts.length && parts.every(part => {
    const [name, interval, ...rest] = part.split(":");
    if (rest.length) return false;
    if (interval === undefined) return pivotField(name!);
    return !measures && pivotField(name!) && ["day", "week", "month", "quarter", "year"].includes(interval);
  });
}
const pivotPath = (value: unknown): boolean => Array.isArray(value) && value.length <= 16
  && value.every(key => typeof key === "string" ? key.length <= 256 : typeof key === "boolean" || Number.isSafeInteger(key));
function pivotJson(value: string): unknown { try { return JSON.parse(value); } catch { return undefined; } }
function pivotOpen(value: string): boolean {
  const parsed = pivotJson(value);
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) return false;
  let paths = 0;
  return Object.entries(parsed).every(([axis, list]) => (axis === "r" || axis === "c") && Array.isArray(list) && (paths += list.length) <= 1024 && list.every(pivotPath));
}
function pivotSort(value: string): boolean {
  const parsed = pivotJson(value);
  return Array.isArray(parsed) && (parsed.length === 0 || parsed.length === 3 && typeof parsed[0] === "string" && (parsed[0] === "__count" || pivotField(parsed[0]))
    && (parsed[1] === "asc" || parsed[1] === "desc") && pivotPath(parsed[2]));
}
function validate(route: Route): void {
  if (typeof route !== "object" || route === null || Array.isArray(route)) return fail("url");
  for (const key of Object.keys(route)) if (!routeKeys.has(key)) return fail("url");
  if (route.action !== undefined) decimalId(route.action, "action");
  for (const key of queryKeys) {
    const value = route[key];
    if (value === undefined) continue;
    transportString(value, key);
    switch (key) {
      case "debug": if (value !== "1") return fail(key); break;
      case "menu": case "id": case "active_id": decimalId(value, key); break;
      case "model":
        if (value.length > 128 || !/^[a-z][a-z0-9_]*(?:\.[a-z][a-z0-9_]*)*$/.test(value)) return fail(key);
        break;
      case "view_type": if (value !== "list" && value !== "form" && value !== "pivot") return fail(key); break;
      case "pivot_rows": case "pivot_cols": if (!pivotSpecs(value, false)) return fail(key); break;
      case "pivot_measures": if (!pivotSpecs(value, true)) return fail(key); break;
      case "pivot_open": if (!pivotOpen(value)) return fail(key); break;
      case "pivot_sort": if (!pivotSort(value)) return fail(key); break;
      case "method_action": {
        const match = /^([0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}):([1-9][0-9]{0,15})$/.exec(value);
        if (!match || BigInt(match[2]!) > BigInt(Number.MAX_SAFE_INTEGER)) return fail(key);
        break;
      }
    }
  }
  if (route.cids !== undefined) {
    if (!Array.isArray(route.cids) || !route.cids.length || route.cids.length > 64) return fail("cids");
    for (const id of route.cids) decimalId(id, "cids");
    if (new Set(route.cids).size !== route.cids.length) return fail("cids");
  }
  if (route.action === undefined && Object.keys(route).some(key => key !== "cids" && key !== "debug" && route[key as keyof Route] !== undefined)) return fail("action");
  if (route.id !== undefined && route.view_type !== "form") return fail("view_type");
}
export function parseRoute(url: URL): Route {
  if (url.href.includes("#")) return fail("hash");
  if (byteLength(url.pathname + url.search) > 2048) return fail("url");
  const route: Route = {};
  const pathKey = /^\/(?:erp|odoo)\/action-/.test(url.pathname) ? "action" : "path";
  const path = decode(url.pathname, pathKey);
  if (path !== "/erp" && path !== "/odoo" && path !== "/web") {
    const match = /^\/(?:erp|odoo)\/action-(.*)$/.exec(path);
    if (!match) return fail("path");
    route.action = decimalId(match[1]!, "action");
  }
  const seen = new Set<string>();
  if (url.search) for (const pair of url.search.slice(1).split("&")) {
    const equal = pair.indexOf("=");
    const key = decode((equal < 0 ? pair : pair.slice(0, equal)).replace(/\+/g, " "), "url");
    if (!routeKeys.has(key)) return fail("url");
    if (equal < 0 || seen.has(key)) return fail(key);
    seen.add(key);
    const value = decode(pair.slice(equal + 1).replace(/\+/g, " "), key);
    transportString(value, key);
    switch (key) {
      case "action": if (route.action !== undefined) return fail(key); route.action = decimalId(value, key); break;
      case "cids": route.cids = value.split(","); break;
      default:
        // The closed key set above admits only declared scalar parameters here; validate narrows values.
        Object.assign(route, { [key]: value });
    }
  }
  validate(route);
  // Parsing must not admit an input that exceeds the bound after canonical encoding expansion.
  routeUrl(route);
  return route;
}
export function routeUrl(route: Route): string {
  validate(route);
  const query = new URLSearchParams();
  for (const key of queryKeys) if (route[key] !== undefined) query.set(key, route[key]!);
  if (route.cids !== undefined) query.set("cids", route.cids.join(","));
  const result = (route.action !== undefined ? `/erp/action-${route.action}` : "/erp") + (query.size ? `?${query}` : "");
  if (byteLength(result) > 2048) return fail("url");
  return result;
}
