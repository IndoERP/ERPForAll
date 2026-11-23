/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { ClientError, type CallOptions } from "../transport.js";
import type { FavoriteItem, FavoriteStore } from "./types.js";
import { FAVORITE_PREFIX } from "./search-model.js";

/** The calls the favorite store makes. */
export interface FavoriteRpc { call(model: string, method: string, args: Record<string, unknown>, options?: CallOptions): Promise<unknown> }

const MAX_FAVORITES = 256;
const invalid = (what: string): never => { throw new ClientError("server", `Invalid ${what}.`); };
const isRecord = (value: unknown): value is Record<string, unknown> => typeof value === "object" && value !== null && !Array.isArray(value);
const strings = (value: unknown, what: string): string[] => Array.isArray(value) && value.every(item => typeof item === "string") ? [...value] : invalid(what);

/** Whether an address's `search_filters` names a favorite (malformed text is left to the search model). */
export function namesFavorite(params: URLSearchParams): boolean {
  let ids: unknown;
  try { ids = JSON.parse(params.get("search_filters") ?? "[]"); } catch { return false; }
  return Array.isArray(ids) && ids.some(id => typeof id === "string" && id.startsWith(FAVORITE_PREFIX));
}

/** One favorite as `ir.filters` `erp_get_favorites` and `create_or_replace` return it. */
export function decodeFavorite(raw: unknown): FavoriteItem {
  if (!isRecord(raw)) return invalid("favorite");
  const { id, name, is_default: isDefault, shared, own, problem } = raw;
  if (typeof id !== "number" || !Number.isSafeInteger(id) || id < 1 || typeof name !== "string" || typeof isDefault !== "boolean" || typeof shared !== "boolean" || typeof own !== "boolean") return invalid("favorite");
  const base = { id: String(id), name, isDefault, isShared: shared, isOwn: own, isActive: false };
  if (problem !== undefined) {
    if (typeof problem !== "string") return invalid("favorite problem");
    return { ...base, domain: [], groupBy: [], sort: [], problem };
  }
  if (!Array.isArray(raw["domain"])) return invalid("favorite domain");
  return { ...base, domain: structuredClone(raw["domain"]), groupBy: strings(raw["group_by"], "favorite grouping"), sort: strings(raw["sort"], "favorite sort") };
}

/** The favorites of `model`'s list under the window action `actionId`, kept in `ir.filters`. */
export function favoriteStore(rpc: FavoriteRpc, model: string, actionId: number, context: Record<string, unknown>): FavoriteStore {
  return {
    load: async () => {
      const rows = await rpc.call("ir.filters", "erp_get_favorites", { model, action_id: actionId, context });
      if (!Array.isArray(rows) || rows.length > MAX_FAVORITES) return invalid("favorite list");
      return rows.map(decodeFavorite);
    },
    save: async draft => decodeFavorite(await rpc.call("ir.filters", "create_or_replace", {
      name: draft.name, model, action_id: actionId, domain: draft.domain, group_by: draft.groupBy, is_default: draft.isDefault, shared: draft.isShared, context
    }, { write: true })),
    remove: async id => { await rpc.call("ir.filters", "unlink", { ids: [Number(id)], context }, { write: true }); }
  };
}
