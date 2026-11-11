/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { createDateTimeEditor, createDateTimeRangeEditor, type DatePickerPrecision, type DateTimeEditorOptions } from "./datetime-editor.js";
import { assertEditorValue, formatEditorValue, partsFromValue, type EditorLocale, type EditorValue } from "./datetime-values.js";
import { button, el } from "./dom.js";
import { absentPlaceholderInput } from "./field-options.js";
import { fieldShell } from "./field-shell.js";
import type { FieldControl, FieldControlOptions } from "./fields.js";
import { ClientError } from "./transport.js";

function valueFor(value: unknown, name: string): EditorValue {
  if (value === false || value === null || value === undefined) return false;
  if (typeof value !== "string") throw new ClientError("server", `Invalid date/time value for ${name}.`);
  return value;
}
function today(locale: EditorLocale): string {
  const parts = new Intl.DateTimeFormat("en-CA", { timeZone: locale.tz, year: "numeric", month: "2-digit", day: "2-digit", numberingSystem: "latn" }).formatToParts(new Date());
  const part = (kind: "year" | "month" | "day"): string => {
    const value = parts.find(item => item.type === kind)?.value;
    if (value === undefined || !/^\d+$/.test(value)) throw new ClientError("server", "The current date could not be formatted in the configured timezone.");
    return value.padStart(kind === "year" ? 4 : 2, "0");
  };
  return `${part("year")}-${part("month")}-${part("day")}`;
}
function bound(value: string | undefined, name: string, locale: EditorLocale): string | undefined {
  if (value === undefined) return undefined;
  const canonical = value === "today" ? today(locale) : value;
  try { assertEditorValue("date", canonical); }
  catch (failure) { if (!(failure instanceof ClientError)) throw failure; throw new ClientError("validation", `Invalid date bound: ${name}.`); }
  return canonical;
}
function precision(value: string | undefined): DatePickerPrecision | undefined {
  if (value === undefined) return undefined;
  if (value !== "days" && value !== "months" && value !== "years" && value !== "decades") throw new ClientError("validation", "Date picker precision must be days, months, years or decades.");
  return value;
}
function warning(value: string | undefined): "none" | "past" | "future" | undefined {
  if (value !== undefined && value !== "none" && value !== "past" && value !== "future") throw new ClientError("validation", "Date warning must be none, past or future.");
  return value;
}

/** Adapt admitted date options without performing an independent record write. */
export function createDateFieldControl(options: FieldControlOptions): FieldControl {
  const { field } = options, config = field.options ?? {}, editor = options.editor;
  if (field.kind !== "date" && field.kind !== "datetime") throw new ClientError("validation", "Date field controls require a date or datetime field.");
  if (!editor || !("locale" in editor)) throw new ClientError("unsupported", "Date/time fields require an editor locale and the shared popover service.");
  const min = bound(config.min_date, "min_date", editor.locale), max = bound(config.max_date, "max_date", editor.locale);
  if (min !== undefined && max !== undefined && min > max) throw new ClientError("validation", "Date minimum must not be after maximum.");
  let placeholder = config.placeholder;
  if (config.placeholder_field) {
    const name = config.placeholder_field.name;
    const absent = absentPlaceholderInput({ name, values: options.values, fields: options.fields });
    if (!options.values || !Object.hasOwn(options.values, name) && !absent) throw new ClientError("unsupported", "Date placeholder requires an available field value.");
    const supplied = options.values[name];
    if (supplied === false || supplied === null || absent) placeholder = "";
    else if (typeof supplied === "string") placeholder = supplied;
    else throw new ClientError("server", "Invalid date placeholder field value.");
  }
  const common: Omit<DateTimeEditorOptions, "name" | "label" | "value" | "onApply"> = {
    kind: field.kind, locale: editor.locale, popovers: editor.popovers, signal: editor.signal,
    // Direct callers retain the existing seconds presentation unless an occurrence declares otherwise.
    showSeconds: config.show_seconds ?? true, rounding: config.rounding ?? 0,
    ...(config.show_time === undefined ? {} : { showTime: config.show_time }),
    ...(config.numeric === undefined ? {} : { numeric: config.numeric }),
    ...(placeholder === undefined ? {} : { placeholder }),
    ...(config.min_precision === undefined ? {} : { minPrecision: precision(config.min_precision) }),
    ...(config.max_precision === undefined ? {} : { maxPrecision: precision(config.max_precision) }),
    ...(config.warn === undefined ? {} : { warn: warning(config.warn) }),
    ...(min === undefined && max === undefined ? {} : { isDateValid: day => (min === undefined || day >= min) && (max === undefined || day <= max) }),
    onInvalid: failure => { if (failure.kind !== "validation" && failure.kind !== "unsupported") editor.report(failure); },
    report: editor.report,
  };
  const readText = (value: EditorValue): string => {
    let locale = editor.locale;
    if (config.numeric !== undefined) locale = { ...locale, date_format: locale.date_format.replace(/%(%|m|b|B)/g, (token, directive: string) => directive === "%" ? token : config.numeric ? "%m" : "%B") };
    if (field.kind === "datetime" && config.show_time === false && value !== false) {
      const parts = partsFromValue("datetime", value, locale);
      if (!parts) throw new ClientError("server", "A represented date/time value has no calendar parts.");
      const date = `${String(parts.year).padStart(4, "0")}-${String(parts.month).padStart(2, "0")}-${String(parts.day).padStart(2, "0")}`;
      return formatEditorValue("date", date, locale);
    }
    return formatEditorValue(field.kind as "date" | "datetime", value, locale, { showSeconds: config.show_seconds ?? true });
  };
  const read = (values: readonly EditorValue[]): FieldControl => {
    const summary = values.every(value => value === false) ? "" : values.map(value => value === false && values.length > 1 ? "—" : readText(value)).join(" – ");
    const node = el("span", summary, "field-read-value");
    if (config.warn && config.warn !== "none") {
      const current = today(editor.locale);
      const warned = values.some(value => {
        if (value === false) return false;
        const parts = partsFromValue(field.kind as "date" | "datetime", value, editor.locale);
        if (!parts) throw new ClientError("server", "A represented date/time value has no calendar parts.");
        const day = `${String(parts.year).padStart(4, "0")}-${String(parts.month).padStart(2, "0")}-${String(parts.day).padStart(2, "0")}`;
        return config.warn === "past" ? day < current : day > current;
      });
      if (warned) { const warning = el("span", "⚠", "field-date-warning"), message = config.warn === "past" ? "This date is in the past." : "This date is in the future."; warning.setAttribute("role", "img"); warning.setAttribute("aria-label", message); warning.title = message; node.append(warning); }
    }
    return { node: fieldShell(field, node, "list").node, dispose: () => {} };
  };
  const startName = config.start_date_field?.name ?? field.name, endName = config.end_date_field?.name ?? field.name;
  const range = field.widget === "daterange" || config.start_date_field !== undefined || config.end_date_field !== undefined;
  if (!range && options.viewContext === "list" && field.readonly) return read([valueFor(options.value, field.name)]);
  if (!range) return createDateTimeEditor({ ...common, name: field.name, label: field.label,
    value: valueFor(options.value, field.name), readonly: field.readonly, required: field.required, onApply: options.change });
  if (startName === endName || startName !== field.name && endName !== field.name) throw new ClientError("validation", "Date range endpoints must be distinct and include the current field.");
  const startSource = startName === field.name ? field : options.fields?.find(item => item.name === startName);
  const endSource = endName === field.name ? field : options.fields?.find(item => item.name === endName);
  const start = startSource ? { ...startSource, readonly: field.readonly || startSource.readonly } : undefined;
  const end = endSource ? { ...endSource, readonly: field.readonly || endSource.readonly } : undefined;
  if (!start || !end) throw new ClientError("unsupported", "Date ranges require both endpoint field descriptions.");
  if (start.kind !== field.kind || end.kind !== field.kind) throw new ClientError("validation", "Date range endpoints must have the same date/time kind.");
  if (!options.values || !Object.hasOwn(options.values, startName) || !Object.hasOwn(options.values, endName)) throw new ClientError("unsupported", "Date ranges require both endpoint values.");
  let represented: readonly [EditorValue, EditorValue] = [valueFor(options.values[startName], startName), valueFor(options.values[endName], endName)];
  if (options.viewContext === "list" && start.readonly && end.readonly) return read(represented);
  if (!options.changeValues && !(start.readonly && end.readonly)) throw new ClientError("unsupported", "Date ranges require an atomic changeValues callback.");
  const apply = options.changeValues ?? (() => { throw new ClientError("validation", "Readonly date ranges cannot be changed."); });
  const handle = createDateTimeRangeEditor({ ...common, names: [startName, endName], labels: [start.label, end.label], value: represented,
    required: [start.required, end.required], readonlyEndpoints: [start.readonly, end.readonly],
    onApply: values => {
      const changes: Record<string, unknown> = {};
      if (values[0] !== represented[0]) changes[startName] = values[0];
      if (values[1] !== represented[1]) changes[endName] = values[1];
      if (!Object.keys(changes).length) return;
      apply(changes); represented = values;
    } });
  const endpoints = handle.node.querySelectorAll<HTMLElement>(":scope > .datetime-editor");
  const startNode = endpoints[0], endNode = endpoints[1];
  if (!startNode || !endNode) { handle.dispose(); throw new ClientError("server", "Date range endpoint controls are missing."); }
  startNode.dataset["rangeEndpoint"] = "start"; endNode.dataset["rangeEndpoint"] = "end";
  if (!config.always_range && represented[1] === false && !end.required) {
    endNode.hidden = true;
    const expand = button(`Add ${end.label}`, () => { endNode.hidden = false; expand.remove(); handle.inputs[1].focus(); });
    expand.dataset["editorAction"] = "expand-range"; expand.disabled = end.readonly;
    handle.node.append(expand);
  }
  return handle;
}
