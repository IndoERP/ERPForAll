/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { DateTime } from "luxon";
import { el } from "./dom.js";
import { valueFromParts, type EditorLocale } from "./datetime-values.js";
import type { CalendarEvent } from "./calendar.js";
import type { CalendarNode } from "./metadata.js";
import { ClientError } from "./transport.js";

/** A keyboard-accessible date range editor. Date stops are inclusive on the wire. */
export function calendarEditor(root: CalendarNode, locale: EditorLocale, initial: DateTime, event: CalendarEvent | undefined,
  save: (values: Record<string, string>, name?: string) => Promise<void>, run: (job: () => Promise<void>) => void): HTMLFormElement {
  const form = el("form", undefined, "calendar-editor");
  const date = root.dateStart.kind === "date";
  const input = (label: string, type: string, value: string): HTMLInputElement => {
    const field = el("label", label), node = el("input");
    node.type = type; node.value = value; node.required = true;
    field.append(node); form.append(field);
    return node;
  };
  const title = !event && root.quickCreate ? input("Event name", "text", "") : undefined;
  const format = date ? "yyyy-MM-dd" : "yyyy-MM-dd'T'HH:mm:ss";
  const start = input("Start", date ? "date" : "datetime-local", initial.toFormat(format));
  const last = event?.allDay ? event.end.minus({ days: 1 }) : event?.end;
  const stop = root.dateStop ? input("End", start.type, (last ?? (date ? initial : initial.plus({ hours: 1 }))).toFormat(format)) : undefined;
  start.step = "1"; if (stop) stop.step = "1";
  start.disabled = root.dateStart.readonly;
  if (stop) stop.disabled = root.dateStop!.readonly;
  // Moving the start also moves the end by the same duration. Editing only End resizes.
  let lastStart = initial;
  start.addEventListener("change", () => {
    const next = DateTime.fromISO(start.value, { zone: locale.tz });
    if (stop && !stop.disabled && next.isValid && lastStart.isValid) {
      const end = DateTime.fromISO(stop.value, { zone: locale.tz });
      if (end.isValid) stop.value = end.plus(date || event?.allDay ? next.diff(lastStart, "days") : next.diff(lastStart)).toFormat(format);
    }
    lastStart = next;
  });
  const submit = el("button", event ? "Save dates" : root.quickCreate ? "Create" : "Continue", "erp-button erp-button-primary");
  submit.type = "submit"; form.append(submit);
  const wire = (input: HTMLInputElement, previous: unknown): string => {
    const parts = DateTime.fromISO(input.value, { zone: "UTC" });
    const { year, month, day, hour, minute, second } = parts;
    const result = valueFromParts(date ? "date" : "datetime", { year, month, day, hour, minute, second }, locale, typeof previous === "string" ? previous : undefined);
    if (!result.ok) throw result.error;
    if (!result.value) throw new ClientError("validation", "Enter a date.");
    return result.value;
  };
  let full: HTMLButtonElement | undefined;
  if (title) {
    full = el("button", "Open form", "erp-button");
    full.type = "submit"; full.formNoValidate = true;
    form.append(full);
  }
  form.addEventListener("submit", e => {
    e.preventDefault(); if (submit.disabled) return;
    run(async () => {
      const from = wire(start, event?.record[root.dateStart.name]);
      const values: Record<string, string> = {};
      if (!start.disabled) values[root.dateStart.name] = from;
      if (root.dateStop && stop) {
        const to = wire(stop, event?.record[root.dateStop.name]);
        if (to < from) throw new ClientError("validation", "The end must be on or after the start.");
        if (!stop.disabled) values[root.dateStop.name] = to;
      }
      submit.disabled = true;
      try { await save(values, full && e.submitter === full ? undefined : title?.value.trim()); } finally { submit.disabled = false; }
    });
  });
  return form;
}
