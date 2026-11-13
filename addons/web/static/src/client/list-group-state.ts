/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { ClientError } from "./transport.js";

/** Page-local state, bounded by the existing shell URL and search-item admission limits. */
export interface ListGroupState { v: 1; by: string[]; offset: number; limit?: number; order: string; open: { path: string; offset: number }[] }
const bad = (): never => { throw new ClientError("validation", "Invalid grouping state."); };
const spec = (value: unknown): value is string => typeof value === "string" && /^[a-z_][a-z0-9_]*(?::(?:day|week|month|quarter|year))?$/.test(value);
const natural = (value: unknown): value is number => typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
const obj = (value: unknown, keys: string[]): Record<string, unknown> => {
  if (!value || typeof value !== "object" || Array.isArray(value) || Object.keys(value).length !== keys.length || Object.keys(value).some(key => !keys.includes(key))) return bad();
  return value as Record<string, unknown>;
};
/** Internal paths admit long server labels; the complete saved state still has the 2048-byte shell bound.
 * Paths are encoded, never executable domains. Group text cannot introduce URL transport hazards. */
export function encodePath(path: unknown): string {
  const bytes = new TextEncoder().encode(JSON.stringify(path));
  if (bytes.length > 32768) return bad();
  return btoa(Array.from(bytes, byte => String.fromCharCode(byte)).join("")).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}
export function decodePath(encoded: string): [string, string | number | boolean][] {
  if (!/^[A-Za-z0-9_-]+$/.test(encoded) || encoded.length > 43692) return bad();
  let value: unknown;
  try { value = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(Uint8Array.from(atob(encoded.replace(/-/g, "+").replace(/_/g, "/")), c => c.charCodeAt(0)))); }
  catch { return bad(); }
  if (!Array.isArray(value) || !value.length || value.length > 32) return bad();
  for (const part of value) {
    if (!Array.isArray(part) || part.length !== 2 || !spec(part[0]) || !(typeof part[1] === "string" || typeof part[1] === "boolean" || typeof part[1] === "number" && Number.isSafeInteger(part[1]))) return bad();
  }
  if (encodePath(value) !== encoded) return bad();
  return value as [string, string | number | boolean][];
}
/** Legacy arrays carry dimensions only; the versioned object additionally carries open branches and paging. */
export function decodeListState(raw: unknown): ListGroupState {
  const value: Record<string, unknown> = Array.isArray(raw) ? { v: 1, by: raw, offset: 0, order: "", open: [] } : obj(raw, ["v", "by", "offset", "order", "open", ...(raw && typeof raw === "object" && "limit" in raw ? ["limit"] : [])]);
  const by = value["by"], open = value["open"], order = value["order"];
  if (value["v"] !== 1 || !Array.isArray(by) || by.length > 32 || !by.every(spec) || new Set(by).size !== by.length || !natural(value["offset"]) || typeof order !== "string" || order.length > 256 || order !== "" && !/^[a-z_][a-z0-9_]* (asc|desc)$/.test(order) || !Array.isArray(open) || open.length > 32) return bad();
  const seen = new Set<string>();
  const opened = open.map(raw => {
    const entry = obj(raw, ["path", "offset"]), path = entry["path"];
    if (typeof path !== "string" || !natural(entry["offset"]) || seen.has(path)) return bad();
    const parts = decodePath(path);
    if (parts.length > by.length || parts.some(([field], index) => field !== by[index])) return bad();
    seen.add(path); return { path, offset: entry["offset"] };
  });
  const limit = value["limit"];
  if (limit !== undefined && (!natural(limit) || limit === 0)) return bad();
  const result: ListGroupState = { v: 1, by: [...by], offset: value["offset"] as number, order, open: opened, ...(typeof limit === "number" ? { limit } : {}) };
  if (new TextEncoder().encode(JSON.stringify(result)).length > 2048) return bad();
  return result;
}
