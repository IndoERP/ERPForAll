/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { array, integer, object, type Field } from "./metadata.js";
import { checkedRelationChoice, type RelationChoice } from "./relation-picker.js";
import { ClientError, type RpcClient } from "./transport.js";
import type { KanbanColumnData } from "./kanban-columns.js";
import type { KanbanColumnKey } from "./kanban-fold.js";

/** A bounded read in catalogue order. Both the relation and its records are read under the caller's rules. */
export async function readCatalogue(rpc: RpcClient, model: string, domain: unknown[], context: Record<string, unknown>, limit: number) {
  const page = object(await rpc.call(model, "web_search_read", { domain, context, specification: { display_name: {} }, limit }), "kanban catalogue");
  const length = integer(page["length"], "catalogue length");
  const records = array(page["records"], "catalogue choices");
  if (records.length !== Math.min(limit, length)) throw new ClientError("server", "Invalid kanban catalogue page length.");
  const choices = records.map(checkedRelationChoice);
  if (new Set(choices.map(choice => choice.id)).size !== choices.length) throw new ClientError("server", "Duplicate kanban catalogue entry.");
  return { choices, length };
}

/** Resolve even a saved URL through a permitted read, so an archived or inaccessible scope never broadens the board. */
export async function readBoardScope(rpc: RpcClient, model: string, field: Field, requested: number | undefined, context: Record<string, unknown>): Promise<RelationChoice | undefined> {
  if (requested === undefined) {
    const defaults = object(await rpc.call(model, "default_get", { fields: [field.name], context }), "board default");
    const value = defaults[field.name];
    if (value !== false && value !== undefined && value !== null) {
      requested = integer(value, "board default id");
      if (!requested) throw new ClientError("server", "Invalid kanban scope id.");
    }
  }
  const domain = requested === undefined ? [] : [["id", "=", requested]];
  const page = await readCatalogue(rpc, field.relation!, domain, context, 1);
  const scope = page.choices[0];
  if (scope && requested !== undefined && scope.id !== requested) throw new ClientError("server", "The scope does not match the requested record.");
  return scope;
}

/** A stage is identified by its id, independently of its name or its count under the current filter. */
export function expandScopedColumns(field: string, catalogue: readonly RelationChoice[], populated: readonly KanbanColumnData[], folded: readonly KanbanColumnKey[]): KanbanColumnData[] {
  const groups = new Map(populated.map(column => [column.value, column]));
  const columns = catalogue.map(choice => {
    const populated = groups.get(choice.id);
    groups.delete(choice.id);
    if (populated) return { ...populated, label: choice.display_name };
    return { key: JSON.stringify(choice.id), value: choice.id, label: choice.display_name, count: 0, domain: [[field, "=", choice.id]],
      sum: 0, ...(folded.includes(choice.id) ? {} : { records: [] }) };
  });
  if (groups.size) throw new ClientError("server", "The column catalogue has changed. Reload the board.");
  return columns;
}
