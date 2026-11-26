/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The grouped kanban's state in the page address (`kanban_fold`, M4.KANBAN-CLIENT-COLUMNS): the columns the user folded
// or opened, for one grouping. A closed, bounded shape; the server admits the same on a reload
// (crates/erp-server/src/redirect.rs `kanban_fold`).
import { ClientError } from "./transport.js";

/** The key of a column: its group value (an id, a selection value, a boolean, `false` for unset, an interval's start). */
export type KanbanColumnKey = string | number | boolean;
/** What the user changed on the board grouped by `by`; a column in neither list follows the server's default. */
export interface KanbanFoldState { by: string; folded: KanbanColumnKey[]; opened: KanbanColumnKey[] }

/** The most columns each list names, the longest text key and the longest encoded value. */
export const KANBAN_FOLD_BOUNDS = { keys: 64, keyLength: 64, length: 1024 } as const;
const SPEC = /^[a-z_][a-z0-9_]{0,127}(?::(?:day|week|month|quarter|year))?$/;
// A text key travels as it is: nothing an address refuses (route.ts `transportString`) and nothing JSON would escape.
const key = (value: unknown): value is KanbanColumnKey => typeof value === "boolean" || typeof value === "number" && Number.isSafeInteger(value)
  || typeof value === "string" && value.length <= KANBAN_FOLD_BOUNDS.keyLength && !/[\u0000-\u001f\u007f-\u009f\\"]/u.test(value) && !value.includes("//");
const keys = (value: unknown): value is KanbanColumnKey[] => Array.isArray(value) && value.length <= KANBAN_FOLD_BOUNDS.keys && value.every(key)
  && new Set(value.map(item => JSON.stringify(item))).size === value.length;

/** True when `raw` is a well-formed `kanban_fold` value (the route's check; it never throws). */
export function validKanbanFold(raw: string): boolean {
  if (raw.length > KANBAN_FOLD_BOUNDS.length) return false;
  let value: unknown;
  try { value = JSON.parse(raw); } catch { return false; }
  if (typeof value !== "object" || value === null || Array.isArray(value)) return false;
  const state = value as Record<string, unknown>, names = Object.keys(state);
  if (names.length !== 3 || !names.every(name => name === "by" || name === "f" || name === "o")) return false;
  const by = state["by"], folded = state["f"], opened = state["o"];
  if (typeof by !== "string" || !SPEC.test(by) || !keys(folded) || !keys(opened)) return false;
  const closed = new Set(folded.map(item => JSON.stringify(item)));
  if (!opened.every(item => !closed.has(JSON.stringify(item)))) return false; // a column is folded or opened, never both
  // Only the text this client writes: members in this order, no spaces, integers written plainly (`1.0`, `1e2` and `-0` are
  // other spellings the server's address policy refuses too).
  return JSON.stringify({ by, f: folded, o: opened }) === raw;
}

/** The state an address carries. A malformed value is refused; the caller drops a state of another grouping. */
export function decodeKanbanFold(raw: string): KanbanFoldState {
  if (!validKanbanFold(raw)) throw new ClientError("validation", "Invalid kanban column state.");
  const state = JSON.parse(raw) as { by: string; f: KanbanColumnKey[]; o: KanbanColumnKey[] };
  return { by: state.by, folded: state.f, opened: state.o };
}

/** The address value of `state`, or nothing when the user changed no column. Too many columns to name is refused with what to do. */
export function encodeKanbanFold(state: KanbanFoldState): string | undefined {
  if (!state.folded.length && !state.opened.length) return undefined;
  const raw = JSON.stringify({ by: state.by, f: state.folded, o: state.opened });
  if (validKanbanFold(raw)) return raw;
  if ([...state.folded, ...state.opened].some(item => !key(item))) throw new ClientError("validation", "This column cannot be folded or opened by hand: its value is too long, or holds characters the page address cannot keep.");
  throw new ClientError("validation", "Too many columns are folded or opened to keep in the page address. Open the board again from its menu to start from its default columns.");
}

/** `state` with the column `column` folded or opened by the user (it leaves the other list). */
export function withKanbanColumn(state: KanbanFoldState, column: KanbanColumnKey, folded: boolean): KanbanFoldState {
  const same = JSON.stringify(column), without = (list: KanbanColumnKey[]): KanbanColumnKey[] => list.filter(item => JSON.stringify(item) !== same);
  return folded ? { by: state.by, folded: [...without(state.folded), column], opened: without(state.opened) }
    : { by: state.by, folded: without(state.folded), opened: [...without(state.opened), column] };
}
