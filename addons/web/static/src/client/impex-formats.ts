/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { ClientError } from "./transport.js";

export const IMPEX_FORMATS = [
  { key: "xlsx", label: "Excel (.xlsx)", mime: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" },
  { key: "csv", label: "CSV (.csv)", mime: "text/csv" },
] as const;
export type ImpexFormat = typeof IMPEX_FORMATS[number]["key"];

export interface ImpexField {
  readonly name: string;
  readonly label: string;
  readonly required: boolean;
}
export interface ImportTemplate {
  readonly label: string;
  readonly format: ImpexFormat;
}

export function impexFormat(value: string): typeof IMPEX_FORMATS[number] {
  const format = IMPEX_FORMATS.find(item => item.key === value);
  if (!format) throw new ClientError("validation", "Choose CSV or Excel (.xlsx).");
  return format;
}

export function decodeImpexFields(raw: unknown): ImpexField[] {
  if (!Array.isArray(raw)) throw new ClientError("server", "The server returned an invalid list of fields.");
  const fields = raw.map(value => {
    if (!value || typeof value !== "object" || typeof value.name !== "string" || !value.name
        || typeof value.label !== "string" || !value.label || typeof value.required !== "boolean") {
      throw new ClientError("server", "The server returned an invalid import or export field.");
    }
    return { name: value.name, label: value.label, required: value.required };
  });
  if (new Set(fields.map(field => field.name)).size !== fields.length) throw new ClientError("server", "The server returned duplicate fields.");
  return fields;
}

export function decodeImportTemplates(raw: unknown): ImportTemplate[] {
  if (!Array.isArray(raw)) throw new ClientError("server", "The server returned an invalid list of templates.");
  return raw.map(value => {
    if (!value || typeof value !== "object" || typeof value.label !== "string" || !value.label
        || (value.format !== "csv" && value.format !== "xlsx")) {
      throw new ClientError("server", "The server returned an invalid import template.");
    }
    return { label: value.label, format: value.format };
  });
}
