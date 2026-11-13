/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { FieldPresentationOptions as RawPresentationOptions } from "../generated/human-ui.js";
import { decodeAst, parseBoundedJson } from "../../../../../web/packages/expr/src/index.js";
import type { Field } from "./metadata.js";
import { ClientError } from "./transport.js";

/** Non-null members after runtime admission; the generated DTO remains authoritative. */
export type FieldPresentationOptions = { [K in keyof RawPresentationOptions]?: Exclude<RawPresentationOptions[K], null> };

const texts = ["help", "placeholder", "autocomplete", "icon", "icon_class", "min_date", "max_date", "min_precision", "max_precision", "warn", "type", "text"];
const flags = ["password", "no_open", "no_create", "no_quick_create", "no_create_edit", "no_symbol", "enable_formatting", "hide_trailing_zeros", "field_digits", "show_time", "show_seconds", "always_range", "numeric", "create", "can_open", "horizontal", "nolabel"];
const integers = ["rows", "max_length", "badge_limit", "tag_limit", "search_threshold", "rounding"];
const references = ["placeholder_field", "currency_field", "start_date_field", "end_date_field", "whitelist_fname", "color_field"];
/** Admit closed typed presentation options. Dependent root fields must be in authorized metadata. */
export function readFieldOptions(raw: Record<string, unknown>, model: string, fields: ReadonlyMap<string, unknown>, relation?: string | null): FieldPresentationOptions {
  for (const [key, value] of Object.entries(raw)) {
    if (texts.includes(key)) {
      if (typeof value !== "string" || new TextEncoder().encode(value).length > 4096) throw new ClientError("server", `Invalid field option: ${key}.`);
    } else if (flags.includes(key)) {
      if (typeof value !== "boolean") throw new ClientError("server", `Invalid field option: ${key}.`);
    } else if (integers.includes(key)) {
      if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0 || value > 4096) throw new ClientError("server", `Invalid field option: ${key}.`);
      if (key === "rows" && (value < 1 || value > 100) || (key === "badge_limit" || key === "tag_limit") && (value < 1 || value > 256)) throw new ClientError("server", `Invalid field option: ${key}.`);
    } else if (references.includes(key)) {
      if (key === "whitelist_fname") throw new ClientError("unsupported", "Selection whitelist field data is not supported.");
      if (value === null || typeof value !== "object" || Array.isArray(value) || Object.keys(value).length !== 2 || !("model" in value) || !("name" in value) || typeof value.name !== "string" || !/^[a-zA-Z_][a-zA-Z0-9_]*$/.test(value.name) || (value.model === model ? !fields.has(value.name) : key !== "color_field" || value.model !== relation)) throw new ClientError("server", `Invalid field dependency: ${key}.`);
    } else if (key === "whitelisted_values" || key === "blacklisted_values") {
      if (!Array.isArray(value) || value.length > 256 || value.some(item => typeof item !== "string" || new TextEncoder().encode(item).length > 256)) throw new ClientError("server", `Invalid field option: ${key}.`);
    } else if (key === "digits") {
      if (!Array.isArray(value) || value.length !== 2 || value.some(item => typeof item !== "number" || !Number.isSafeInteger(item) || item < 0 || item > 20) || value[1] > value[0]) throw new ClientError("server", "Invalid field option: digits.");
    } else if (key === "domain" || key === "context") {
      decodeAst(parseBoundedJson(JSON.stringify(value)));
    } else throw new ClientError("unsupported", `Unknown field option: ${key}.`);
  }
  return raw as FieldPresentationOptions;
}

/** An omitted default is unset only when the owner supplied an authorized text-field descriptor.
 * Keep record values untouched; unknown references and explicitly malformed values still fail. */
export function absentPlaceholderInput({ name, values, fields }: { name: string; values?: Readonly<Record<string, unknown>>; fields?: readonly Field[] }): boolean {
  return values !== undefined && !Object.hasOwn(values, name) && fields?.some(field => field.name === name && (field.kind === "char" || field.kind === "text")) === true;
}
