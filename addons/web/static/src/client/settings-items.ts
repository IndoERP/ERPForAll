/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { SettingsItem, SettingsLinkTarget } from "../generated/human-ui.js";
import { array, integer, object, text } from "./metadata.js";
import { ClientError } from "./transport.js";

/** Authorized root-menu presentation in server order. Module is provenance, never authority. */
export interface SettingsRootMenu { readonly id: number; readonly module: string | null; readonly label: string; readonly icon: string | null }
/** Shell-owned dispatch preserves action authorization and the current service's departure guard. */
export interface SettingsNavigation { readonly roots: readonly SettingsRootMenu[]; openTarget(target: SettingsLinkTarget): Promise<void> }
/** Additional Settings host capabilities; standalone scalar-only pages need none. */
export interface SettingsViewOptions { readonly navigation?: SettingsNavigation }

function closed(row: Record<string, unknown>, fields: readonly string[], label: string): void {
  for (const key of Object.keys(row)) if (!fields.includes(key)) throw new ClientError("unsupported", `Unknown ${label} member: ${key}.`);
  for (const key of fields) if (!Object.hasOwn(row, key)) throw new ClientError("server", `Missing ${label} member: ${key}.`);
}
function caption(raw: unknown, label: string, max: number, multiline = false): string {
  const value = text(raw, label);
  if ((!multiline && !value.length) || new TextEncoder().encode(value).length > max || [...value].some(c => /[\u0000-\u001f\u007f-\u009f]/u.test(c) && !(multiline && c === "\n"))) throw new ClientError("server", `Invalid ${label}.`);
  return value;
}
function targetId(raw: unknown): number {
  const id = integer(raw, "setting target id");
  if (!id) throw new ClientError("server", "Invalid setting target id.");
  return id;
}
/** Strict runtime decoding of the Rust-owned target union; no target authorizes a route. */
export function readSettingsTarget(raw: unknown): SettingsLinkTarget {
  const row = object(raw, "setting target");
  if (row["kind"] === "menu") {
    closed(row, ["kind", "id"], "setting target");
    return { kind: "menu", id: targetId(row["id"]) };
  }
  if (row["kind"] === "action") {
    closed(row, ["kind", "id", "res_id"], "setting target");
    return { kind: "action", id: targetId(row["id"]), res_id: row["res_id"] === null ? null : targetId(row["res_id"]) };
  }
  throw new ClientError("unsupported", `Unsupported setting target kind: ${String(row["kind"])}.`);
}
/** Absent items is the explicit major-1 compatibility rule. */
export function readSettingsItems(description: Record<string, unknown>, declarationKeys: readonly string[]): SettingsItem[] {
  if (!Object.hasOwn(description, "items")) return [];
  const keys = new Set(declarationKeys);
  return array(description["items"], "setting items").map(raw => {
    const row = object(raw, "setting item"), kind = row["kind"];
    if (kind !== "info" && kind !== "link") throw new ClientError("unsupported", `Unsupported setting item kind: ${String(kind)}.`);
    closed(row, ["kind", "key", "label", "app", "block", "sequence", ...(kind === "info" ? ["value", "detail"] : ["target"])], "setting item");
    const key = caption(row["key"], "setting item key", 128), app = caption(row["app"], "setting item app", 128);
    if (!/^[a-z][a-z0-9_]*\.[a-z][a-z0-9_]*$/.test(key) || !/^[a-z][a-z0-9_]*$/.test(app)) throw new ClientError("server", "Invalid setting item key or app.");
    if (keys.has(key)) throw new ClientError("server", `Duplicate settings item or declaration key: ${key}.`);
    keys.add(key);
    const label = caption(row["label"], "setting item label", 256), block = caption(row["block"], "setting item block", 256), sequence = integer(row["sequence"], "setting item sequence");
    if (sequence > 0xffff_ffff) throw new ClientError("server", "Invalid setting item sequence.");
    const common = { key, label, app, block, sequence };
    if (kind === "link") return { ...common, kind, target: readSettingsTarget(row["target"]) };
    const value = row["value"];
    if (typeof value !== "boolean" && typeof value !== "string" && !(typeof value === "number" && Number.isFinite(value) && Math.abs(value) <= Number.MAX_SAFE_INTEGER)) throw new ClientError("server", "Invalid setting item value.");
    // A text value is as bounded as every other caption (it may be empty).
    if (typeof value === "string" && value.length) caption(value, "setting item value", 1024);
    return { ...common, kind, value, detail: row["detail"] === null ? null : caption(row["detail"], "setting item detail", 1024, true) };
  });
}
/** Required nullable menu provenance, consumed without inventing an owning addon. */
export function readMenuModule(raw: unknown): string | null {
  if (raw === null) return null;
  const module = text(raw, "menu module");
  if (!module.length) throw new ClientError("server", "Invalid menu module.");
  return module;
}
