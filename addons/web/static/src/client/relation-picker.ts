/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { array, integer, object, text } from "./metadata.js";
import { ClientError, type RpcClient } from "./transport.js";

export interface RelationChoice { readonly id: number; readonly display_name: string }
export interface RelationSearchOptions {
  readonly rpc: RpcClient; readonly model: string; readonly name: string;
  readonly domain: readonly unknown[]; readonly context: Record<string, unknown>; readonly signal?: AbortSignal;
}
/** The shared name-search protocol, with caller-owned domain/context and bounded dialog results. */
export async function searchRelation(options: RelationSearchOptions): Promise<readonly RelationChoice[]> {
  if (!Array.isArray(options.domain)) throw new ClientError("validation", "Relation domain must be an array.");
  const result = array(await options.rpc.call(options.model, "web_name_search", {
    name: options.name, domain: options.domain, context: options.context,
    specification: { display_name: {} }, limit: 80,
  }, options.signal ? { signal: options.signal } : {}), "relation choices");
  if (result.length > 80) throw new ClientError("server", "Relation search returned more than 80 choices.");
  const seen = new Set<number>();
  return result.map(raw => {
    const row = object(raw, "relation choice");
    const id = integer(row["id"], "relation id"), display_name = text(row["display_name"], "relation name");
    if (id === 0) throw new ClientError("server", "Invalid relation id.");
    if (seen.has(id)) throw new ClientError("server", "Relation search returned duplicate choices.");
    if (new TextEncoder().encode(display_name).length > 8192) throw new ClientError("server", "Relation choice name exceeds 8192 bytes.");
    seen.add(id); return { id, display_name };
  });
}

export function checkedRelationChoice(raw: unknown): RelationChoice {
  const value = object(raw, "saved relation choice");
  const id = integer(value["id"], "saved relation id"), display_name = text(value["display_name"], "saved relation name");
  if (id === 0) throw new ClientError("server", "Invalid saved relation id.");
  if (new TextEncoder().encode(display_name).length > 8192) throw new ClientError("server", "Saved relation name exceeds 8192 bytes.");
  return { id, display_name };
}
