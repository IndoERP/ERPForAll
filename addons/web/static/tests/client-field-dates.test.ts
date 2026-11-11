/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { createDateFieldControl } from "../src/client/field-dates.js";
import { createDateTimeEditor, type DateTimeEditorOptions } from "../src/client/datetime-editor.js";
import { PopoverService } from "../src/client/popover.js";
import { RpcClient } from "../src/client/transport.js";
import type { Field } from "../src/client/metadata.js";
import type { FieldControlOptions } from "../src/client/fields.js";

const locale = { lang: "en_GB", tz: "UTC", date_format: "%d/%m/%Y", time_format: "%H:%M:%S" };
const disposers: (() => void)[] = [];
afterEach(() => { for (const dispose of disposers.splice(0).reverse()) dispose(); document.body.replaceChildren(); vi.useRealTimers(); });
const base = (name = "date", kind: "date" | "datetime" = "date"): Field => ({ name, kind, label: name === "date" ? "Date" : name, required: false, readonly: false, selection: null, relation: null, relationField: null });
function fixture(field: Field = base(), value: unknown = "2026-11-04", extra: Partial<FieldControlOptions> = {}) {
  const service = new PopoverService(document.body), change = vi.fn(), changeValues = vi.fn(), report = vi.fn();
  disposers.push(() => service.dispose());
  const handle = createDateFieldControl({ field, value, rpc: new RpcClient(fetch, () => {}), context: {}, change, changeValues, run: job => { void job().catch(report); }, editor: { locale, popovers: service, report }, ...extra });
  disposers.push(handle.dispose); document.body.append(handle.node);
  return { ...handle, change, changeValues, report, input: handle.node.querySelector<HTMLInputElement>("input")! };
}
function direct(extra: Partial<DateTimeEditorOptions> = {}) {
  const service = new PopoverService(document.body), change = vi.fn(), invalid = vi.fn(), report = vi.fn(); disposers.push(() => service.dispose());
  const handle = createDateTimeEditor({ kind: "date", name: "date", label: "Date", value: "2026-11-04", locale, popovers: service, onApply: change, onInvalid: invalid, report, ...extra });
  disposers.push(handle.dispose); document.body.append(handle.node); return { ...handle, change, invalid, report };
}
function edit(input: HTMLInputElement, value: string): void { input.value = value; input.dispatchEvent(new Event("input", { bubbles: true })); input.dispatchEvent(new Event("change", { bubbles: true })); }
function open(node: HTMLElement): void { node.querySelector<HTMLButtonElement>('[aria-haspopup="dialog"]')!.click(); }
const failure = (action: () => unknown, kind: string, message: string): void => { expect(action).toThrowError(expect.objectContaining({ kind, message })); };

/// (partial: date field options and paired canonical changes; browser/served metadata separate)
test("date field forwards declared placeholder and calendar-day bounds", () => {
  const control = fixture({ ...base(), options: { placeholder: "Choose a day", min_date: "2026-11-03", max_date: "2026-11-08" } });
  expect(control.input.placeholder).toBe("Choose a day"); open(control.node);
  expect(document.querySelector<HTMLButtonElement>('[data-date="2026-11-02"]')!.disabled).toBe(true);
  expect(document.querySelector<HTMLButtonElement>('[data-date="2026-11-03"]')!.disabled).toBe(false);
  expect(document.querySelector<HTMLButtonElement>('[data-date="2026-11-09"]')!.disabled).toBe(true);
  edit(control.input, "10/11/2026"); expect(control.change).not.toHaveBeenCalled();
  expect(control.input.checkValidity()).toBe(false);
  expect(control.node.querySelector('.editor-validation')?.textContent).toBe("Date/time value is outside the permitted range.");
});

test("date today bounds and warnings use configured timezone without changing values", () => {
  vi.useFakeTimers(); vi.setSystemTime(new Date("2026-11-04T23:30:00Z"));
  const service = new PopoverService(document.body); disposers.push(() => service.dispose());
  const control = fixture({ ...base(), options: { min_date: "today", warn: "future" } }, "2026-11-06", { editor: { locale: { ...locale, tz: "Asia/Jakarta" }, popovers: service, report: vi.fn() } });
  expect(control.node.querySelector('[data-date-warning]')?.textContent).toBe("This date is in the future.");
  expect(control.input.checkValidity()).toBe(true); expect(control.change).not.toHaveBeenCalled();
  edit(control.input, "04/11/2026"); expect(control.input.checkValidity()).toBe(false); expect(control.change).not.toHaveBeenCalled();
  edit(control.input, "05/11/2026"); expect(control.change).toHaveBeenCalledExactlyOnceWith("2026-11-05");
  expect(control.node.querySelector('[data-date-warning]')?.textContent).toBe("");
});

test("date placeholder dependencies remain separate from the literal placeholder", () => {
  const control = fixture({ ...base(), options: { placeholder: "Literal", placeholder_field: { model: "demo", name: "hint" } } }, false, { values: { hint: "From the record" } });
  expect(control.input.placeholder).toBe("From the record");
  failure(() => fixture({ ...base(), options: { placeholder_field: { model: "demo", name: "hint" } } }, false), "unsupported", "Date placeholder requires an available field value.");
});

test("datetime field can hide its time controls while preserving local clock and stored UTC", () => {
  const control = fixture({ ...base("appointment", "datetime"), options: { show_time: false } }, "2026-11-04 10:15:47");
  expect(control.input.value).toBe("04/11/2026"); open(control.node); expect(document.querySelector('.time-editor')).toBeNull();
  document.querySelector<HTMLButtonElement>('[data-date="2026-11-05"]')!.click();
  expect(control.change).toHaveBeenCalledExactlyOnceWith("2026-11-05 10:15:47");
});

test("datetime hidden clock preserves an unchanged repeated-hour instant and refuses a new DST gap", () => {
  const service = new PopoverService(document.body); disposers.push(() => service.dispose());
  const editor = { locale: { ...locale, tz: "America/New_York" }, popovers: service, report: vi.fn() };
  const repeated = fixture({ ...base("appointment", "datetime"), options: { show_time: false } }, "2026-11-01 06:30:00", { editor });
  edit(repeated.input, "01/11/2026"); expect(repeated.change).not.toHaveBeenCalled();
  const gap = fixture({ ...base("appointment", "datetime"), options: { show_time: false } }, "2026-03-07 07:30:00", { editor });
  edit(gap.input, "08/03/2026"); expect(gap.change).not.toHaveBeenCalled(); expect(gap.input.checkValidity()).toBe(false);
  expect(gap.node.querySelector('.editor-validation')?.textContent).toBe("This local time does not exist in America/New_York.");
});

test("datetime forwards seconds and minute rounding to its time editor", () => {
  const control = fixture({ ...base("appointment", "datetime"), options: { show_seconds: false, rounding: 15 } }, "2026-11-04 10:00:47");
  expect(control.input.value).toBe("04/11/2026 10:00"); open(control.node);
  const time = document.querySelector<HTMLInputElement>('.time-editor input')!; edit(time, "10:08");
  document.querySelector<HTMLButtonElement>('[data-editor-action="apply"]')!.click();
  expect(control.change).toHaveBeenCalledExactlyOnceWith("2026-11-04 10:15:47");
});

test("date numeric false uses named months while retaining declared field order", () => {
  const control = fixture({ ...base(), options: { numeric: false } }); expect(control.input.value).toBe("04/November/2026");
  edit(control.input, "05/November/2026"); expect(control.change).toHaveBeenCalledExactlyOnceWith("2026-11-05");
});

test("date picker month minimum selects a canonical first day and keyboard navigation does not apply", () => {
  const control = fixture({ ...base(), options: { min_precision: "months", max_precision: "years" } }); open(control.node);
  expect(document.querySelector('[data-calendar-precision="months"]')).not.toBeNull();
  const november = document.querySelector<HTMLButtonElement>('[data-period-start="2026-11-01"]')!; november.focus();
  november.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowRight", bubbles: true, cancelable: true }));
  expect((document.activeElement as HTMLElement).dataset['periodStart']).toBe("2026-12-01"); expect(control.change).not.toHaveBeenCalled();
  (document.activeElement as HTMLButtonElement).click(); expect(control.change).toHaveBeenCalledExactlyOnceWith("2026-12-01");
});

test("date picker limits zoom-out and drills from decades through years to months", () => {
  const control = direct({ minPrecision: "months", maxPrecision: "decades" }); open(control.node);
  document.querySelector<HTMLButtonElement>('[data-editor-action="zoom-out"]')!.click();
  expect(document.querySelector('[data-calendar-precision="years"]')).not.toBeNull();
  document.querySelector<HTMLButtonElement>('[data-editor-action="zoom-out"]')!.click();
  expect(document.querySelector('[data-calendar-precision="decades"]')).not.toBeNull();
  expect(document.querySelector('[data-editor-action="zoom-out"]')).toBeNull();
  document.querySelector<HTMLButtonElement>('[data-period-start="2030-01-01"]')!.click();
  expect(document.querySelector('[data-calendar-precision="years"]')).not.toBeNull();
  document.querySelector<HTMLButtonElement>('[data-period-start="2032-01-01"]')!.click();
  expect(document.querySelector('[data-calendar-precision="months"]')).not.toBeNull();
  document.querySelector<HTMLButtonElement>('[data-period-start="2032-02-01"]')!.click();
  expect(control.change).toHaveBeenCalledExactlyOnceWith("2032-02-01");
});

test("date range stages one atomic map with independent required endpoint metadata", () => {
  const start = { ...base("start"), widget: "daterange", options: { end_date_field: { model: "demo", name: "end" }, always_range: true } } satisfies Field;
  const end = { ...base("end"), required: true };
  const control = fixture(start, "2026-11-04", { fields: [start, end], values: { start: "2026-11-04", end: "2026-11-08" } });
  const inputs = control.node.querySelectorAll<HTMLInputElement>('input'); expect(inputs.length).toBe(2); expect(inputs[1]!.required).toBe(true);
  edit(inputs[0]!, "05/11/2026"); expect(control.change).not.toHaveBeenCalled(); expect(control.changeValues).toHaveBeenCalledExactlyOnceWith({ start: "2026-11-05" });
  edit(inputs[1]!, "03/11/2026"); expect(control.changeValues).toHaveBeenCalledTimes(1); expect(inputs[1]!.checkValidity()).toBe(false);
  edit(inputs[1]!, "09/11/2026"); expect(control.changeValues).toHaveBeenLastCalledWith({ end: "2026-11-09" });
});

test("date range preserves an invalid sibling endpoint until it is repaired", () => {
  const start = { ...base("start"), widget: "daterange", options: { end_date_field: { model: "demo", name: "end" }, always_range: true } } satisfies Field, end = base("end");
  const control = fixture(start, "2026-11-04", { fields: [start, end], values: { start: "2026-11-04", end: "2026-11-08" } });
  const inputs = control.node.querySelectorAll<HTMLInputElement>('input'); edit(inputs[1]!, "bad"); expect(inputs[1]!.checkValidity()).toBe(false);
  edit(inputs[0]!, "05/11/2026"); expect(control.changeValues).not.toHaveBeenCalled(); expect(inputs[1]!.checkValidity()).toBe(false);
  edit(inputs[1]!, "09/11/2026"); expect(control.changeValues).toHaveBeenCalledExactlyOnceWith({ start: "2026-11-05", end: "2026-11-09" });
});

test("date range can repair an invalid sibling to its unchanged represented value", () => {
  const start = { ...base("start"), widget: "daterange", options: { end_date_field: { model: "demo", name: "end" }, always_range: true } } satisfies Field, end = base("end");
  const control = fixture(start, "2026-11-04", { fields: [start, end], values: { start: "2026-11-04", end: "2026-11-08" } });
  const inputs = control.node.querySelectorAll<HTMLInputElement>('input'); edit(inputs[1]!, "bad"); edit(inputs[0]!, "05/11/2026");
  expect(control.changeValues).not.toHaveBeenCalled(); edit(inputs[1]!, "08/11/2026");
  expect(control.changeValues).toHaveBeenCalledExactlyOnceWith({ start: "2026-11-05" });
});

test("date range readonly endpoint never changes and optional second endpoint can be revealed", () => {
  const start = { ...base("start"), widget: "daterange", options: { end_date_field: { model: "demo", name: "end" }, always_range: true } } satisfies Field, end = { ...base("end"), readonly: true };
  const control = fixture(start, "2026-11-04", { fields: [start, end], values: { start: "2026-11-04", end: "2026-11-08" } });
  const inputs = control.node.querySelectorAll<HTMLInputElement>('input'); expect(inputs[1]!.disabled).toBe(true); edit(inputs[1]!, "10/11/2026"); expect(control.changeValues).not.toHaveBeenCalled();
  edit(inputs[0]!, "05/11/2026"); expect(control.changeValues).toHaveBeenCalledExactlyOnceWith({ start: "2026-11-05" });
  const optional = fixture({ ...start, options: { end_date_field: { model: "demo", name: "end" }, always_range: false } }, "2026-11-04", { fields: [start, base("end")], values: { start: "2026-11-04", end: false } });
  expect(optional.node.querySelector<HTMLElement>('[data-range-endpoint="end"]')!.hidden).toBe(true);
  optional.node.querySelector<HTMLButtonElement>('[data-editor-action="expand-range"]')!.click();
  expect(optional.node.querySelector<HTMLElement>('[data-range-endpoint="end"]')!.hidden).toBe(false); expect(optional.changeValues).not.toHaveBeenCalled();
});

test("date range refuses missing caller capabilities and malformed endpoint declarations", () => {
  const start = { ...base("start"), widget: "daterange", options: { end_date_field: { model: "demo", name: "end" } } } satisfies Field, end = base("end");
  failure(() => fixture(start, false, { fields: [start, end], values: { start: false, end: false }, changeValues: undefined }), "unsupported", "Date ranges require an atomic changeValues callback.");
  failure(() => fixture(start, false, { values: { start: false, end: false } }), "unsupported", "Date ranges require both endpoint field descriptions.");
  failure(() => fixture(start, false, { fields: [start, end] }), "unsupported", "Date ranges require both endpoint values.");
  failure(() => fixture({ ...start, options: { end_date_field: { model: "demo", name: "start" } } }, false, { fields: [start, end], values: { start: false } }), "validation", "Date range endpoints must be distinct and include the current field.");
});

test("date option errors are typed and disposal cannot commit", () => {
  failure(() => fixture({ ...base(), options: { min_precision: "years", max_precision: "months" } }), "validation", "Date picker minimum precision must not exceed maximum precision.");
  failure(() => fixture({ ...base(), options: { warn: "sometimes" } }), "validation", "Date warning must be none, past or future.");
  failure(() => fixture({ ...base(), options: { min_date: "2026-11-09", max_date: "2026-11-01" } }), "validation", "Date minimum must not be after maximum.");
  const control = fixture(); control.input.value = "06/11/2026"; control.dispose(); control.input.dispatchEvent(new Event("change", { bubbles: true })); expect(control.change).not.toHaveBeenCalled();
});

/// (partial: readonly paired list presentation requires metadata but no mutation capability)
test("readonly date ranges render populated endpoints without an atomic writer", () => {
  const start = { ...base("start"), readonly: true, widget: "daterange", options: { end_date_field: { model: "demo", name: "end" } } };
  const end = base("end");
  const control = fixture(start, "2026-11-04", { fields: [start, end], values: { start: "2026-11-04", end: "2026-11-05" }, changeValues: undefined });
  expect(control.node.querySelectorAll("input")).toHaveLength(2);
  for (const input of control.node.querySelectorAll<HTMLInputElement>("input")) expect(input.disabled || input.readOnly).toBe(true);
  expect(control.change).not.toHaveBeenCalled();
});
