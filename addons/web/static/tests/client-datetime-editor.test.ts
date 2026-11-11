/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { createDateTimeEditor, createDateTimeRangeEditor, type DateTimeEditorHandle } from "../src/client/datetime-editor.js";
import { PopoverService } from "../src/client/popover.js";

const locale = { lang: "en_US", tz: "UTC", date_format: "%m/%d/%Y", time_format: "%H:%M:%S" };
let service: PopoverService | undefined, editor: DateTimeEditorHandle | undefined;
afterEach(() => { editor?.dispose(); service?.dispose(); editor = undefined; service = undefined; document.body.replaceChildren(); vi.useRealTimers(); vi.restoreAllMocks(); });
function fixture(options: Partial<Parameters<typeof createDateTimeEditor>[0]> = {}) {
  service = new PopoverService(document.body); const onApply = vi.fn(), onInvalid = vi.fn(), report = vi.fn();
  editor = createDateTimeEditor({ kind: "date", name: "date", label: "Date", value: "2026-11-03", locale, popovers: service, onApply, onInvalid, report, ...options });
  document.body.append(editor.node); return { ...editor, onApply, onInvalid, report, service };
}
function key(input: HTMLElement, value: string, shiftKey = false): void { input.dispatchEvent(new KeyboardEvent("keydown", { key: value, shiftKey, bubbles: true, cancelable: true })); }
function open(node: HTMLElement): void { node.querySelector<HTMLButtonElement>('[aria-haspopup="dialog"]')!.click(); }

/// (partial: typed apply/restore and departure; calendar and phone routes are separate)
test("date Enter, change and Tab apply only parsed changes, Escape and conversion failures restore", () => {
  const { input, onApply, onInvalid } = fixture(); expect(input.value).toBe("11/03/2026");
  input.value = "11/04/2026"; key(input, "Enter"); expect(onApply).toHaveBeenLastCalledWith("2026-11-04");
  input.dispatchEvent(new Event("change", { bubbles: true })); expect(onApply).toHaveBeenCalledTimes(1);
  input.value = "11/05/2026"; key(input, "Tab"); expect(onApply).toHaveBeenLastCalledWith("2026-11-05");
  input.value = "bad"; key(input, "Escape"); expect(input.value).toBe("11/05/2026"); expect(onApply).toHaveBeenCalledTimes(2);
  input.value = "02/31/2026"; key(input, "Enter"); expect(input.value).toBe("11/05/2026"); expect(onApply).toHaveBeenCalledTimes(2);
  expect(onInvalid).toHaveBeenCalledWith(expect.objectContaining({ kind: "validation", message: "Enter a valid date." }));
});

/// (partial: calendar pointer/keyboard/bounds/today/clear; compiled styling separate)
test("date calendar navigates month and year, refuses limits, selects once, and clears", () => {
  const { node, input, onApply } = fixture({ min: "2026-11-02", max: "2026-12-31" }); open(node);
  expect(document.querySelector('[role="grid"]')).not.toBeNull(); const disabled = document.querySelector<HTMLButtonElement>('[data-date="2026-11-01"]'); expect(disabled?.disabled).toBe(true); disabled!.click(); expect(onApply).not.toHaveBeenCalled();
  const active = document.querySelector<HTMLElement>('[data-date="2026-11-03"]')!; active.focus(); key(active, "ArrowRight"); key(document.activeElement as HTMLElement, "Enter");
  expect(onApply).toHaveBeenLastCalledWith("2026-11-04"); expect(document.querySelector('.message-popover')).toBeNull(); expect(document.activeElement).toBe(input);
  open(node); document.querySelector<HTMLButtonElement>('[aria-label="Next month"]')!.click(); expect(document.querySelector('[data-date="2026-12-01"]')).not.toBeNull();
  document.querySelector<HTMLButtonElement>('[aria-label="Previous year"]')!.click(); expect(document.querySelector('[data-date="2025-12-01"]')).not.toBeNull();
  document.querySelector<HTMLButtonElement>('[data-editor-action="clear"]')!.click(); expect(onApply).toHaveBeenLastCalledWith(false);
});

test("date Today uses user timezone, required stays native, and readonly never opens", () => {
  vi.useFakeTimers(); vi.setSystemTime(new Date("2026-11-03T23:30:00Z")); const current = fixture({ required: true, locale: { ...locale, tz: "Asia/Jakarta" } });
  expect(current.input.required).toBe(true); open(current.node); document.querySelector<HTMLButtonElement>('[data-editor-action="today"]')!.click(); expect(current.onApply).toHaveBeenLastCalledWith("2026-11-04");
  editor!.dispose(); const readonly = fixture({ readonly: true }); expect(readonly.input.disabled).toBe(true); expect(readonly.node.querySelector('[aria-haspopup="dialog"]')).toBeNull();
});

/// (partial: composed date/time and UTC apply; timezone codec and range tests separate)
test("datetime composes date/time draft and applies atomically with seconds preserved", () => {
  const { node, onApply } = fixture({ kind: "datetime", value: "2026-11-03 03:15:42", locale: { ...locale, tz: "Asia/Jakarta" } }); open(node);
  document.querySelector<HTMLButtonElement>('[data-date="2026-11-04"]')!.click(); expect(onApply).not.toHaveBeenCalled();
  const time = document.querySelector<HTMLInputElement>('.datetime-calendar .time-editor input')!; time.value = "11:20"; key(time, "Enter");
  document.querySelector<HTMLButtonElement>('[data-editor-action="apply"]')!.click(); expect(onApply).toHaveBeenCalledTimes(1); expect(onApply).toHaveBeenLastCalledWith("2026-11-04 04:20:42");
});

test("datetime Escape, navigation, abort and parent redraw cannot apply drafts", () => {
  const abort = new AbortController(); const { node, input, onApply, service } = fixture({ signal: abort.signal }); open(node); input.value = "12/10/2026";
  key(input, "Escape"); expect(input.value).toBe("11/03/2026"); expect(onApply).not.toHaveBeenCalled();
  open(node); input.value = "12/10/2026"; service.navigation(); expect(onApply).not.toHaveBeenCalled();
  open(node); abort.abort(); input.dispatchEvent(new Event("change", { bubbles: true })); expect(onApply).not.toHaveBeenCalled(); expect(document.querySelector('.message-popover')).toBeNull();
});

test("date phone sheet is shared and nonmodal; parent setValue is not a user edit", () => {
  vi.spyOn(window, "innerWidth", "get").mockReturnValue(390); const { node, input, setValue, onApply } = fixture();
  setValue("2026-12-02"); expect(input.value).toBe("12/02/2026"); expect(onApply).not.toHaveBeenCalled(); open(node);
  const overlay = document.querySelector<HTMLElement>('.message-popover')!; expect(overlay.dataset["presentation"]).toBe("sheet"); expect(overlay.getAttribute("aria-modal")).not.toBe("true");
});

test("date blur applies a valid typed value without stealing the next control's focus", async () => {
  const { input, onApply } = fixture(); const next = document.createElement("button"); next.textContent = "Continue"; document.body.append(next);
  input.focus(); input.value = "11/10/2026"; next.focus(); await Promise.resolve();
  expect(onApply).toHaveBeenLastCalledWith("2026-11-10"); expect(document.activeElement).toBe(next);
});

test("date keyboard calendar Home End and PageDown move focus without applying", () => {
  const { node, onApply } = fixture(); open(node); const active = document.querySelector<HTMLElement>('[data-date="2026-11-03"]')!; active.focus();
  key(active, "Home"); expect((document.activeElement as HTMLElement).dataset["date"]).toBe("2026-11-02");
  key(document.activeElement as HTMLElement, "End"); expect((document.activeElement as HTMLElement).dataset["date"]).toBe("2026-11-08");
  key(document.activeElement as HTMLElement, "PageDown"); expect((document.activeElement as HTMLElement).dataset["date"]).toBe("2026-12-08"); expect(onApply).not.toHaveBeenCalled();
});

test("date invalid bounds fail loudly and callback failures reach the report channel", () => {
  service = new PopoverService(document.body);
  expect(() => createDateTimeEditor({ kind: "date", name: "date", label: "Date", value: false, locale, popovers: service!, min: "2026-12-01", max: "2026-11-01", onApply: vi.fn(), report: vi.fn() })).toThrowError(expect.objectContaining({ kind: "validation", message: "Date/time minimum must not be after maximum." }));
  const failure = new Error("Owner change failed"), report = vi.fn(); const control = fixture({ onApply: () => { throw failure; }, report });
  control.input.value = "11/08/2026"; key(control.input, "Enter"); expect(report).toHaveBeenLastCalledWith(failure);
});

test("outside dismissal never applies a changed date when its composed time draft is invalid", () => {
  const control = fixture({ kind: "datetime", value: "2026-11-03 10:15:00" }); open(control.node);
  document.querySelector<HTMLButtonElement>('[data-date="2026-11-04"]')!.click();
  const timeInput = document.querySelector<HTMLInputElement>('.datetime-calendar .time-editor input')!;
  timeInput.value = "25:80"; timeInput.dispatchEvent(new Event("input", { bubbles: true })); timeInput.dispatchEvent(new Event("change", { bubbles: true }));
  document.body.dispatchEvent(new MouseEvent("pointerdown", { bubbles: true }));
  expect(control.onApply).not.toHaveBeenCalled(); expect(control.input.value).toBe("11/03/2026 10:15");
  expect(control.onInvalid).toHaveBeenLastCalledWith(expect.objectContaining({ kind: "validation", message: "Enter a valid time." })); expect(document.querySelector('.message-popover')).toBeNull();
});

test("datetime outside departure parses pending embedded time before removing the picker", () => {
  const control = fixture({ kind: "datetime", value: "2026-11-03 10:15:47", showSeconds: false }); open(control.node);
  document.querySelector<HTMLButtonElement>('[data-date="2026-11-04"]')!.click();
  const timeInput = document.querySelector<HTMLInputElement>('.datetime-calendar .time-editor input')!; timeInput.value = "11:20"; timeInput.dispatchEvent(new Event("input", { bubbles: true }));
  document.body.dispatchEvent(new MouseEvent("pointerdown", { bubbles: true })); expect(control.onApply).toHaveBeenCalledExactlyOnceWith("2026-11-04 11:20:47"); expect(document.querySelector('.message-popover')).toBeNull();
  control.input.dispatchEvent(new Event("change", { bubbles: true })); expect(control.onApply).toHaveBeenCalledTimes(1);
});

test("datetime outside departure preserves complete main-text edits while the calendar is open", () => {
  const control = fixture({ kind: "datetime", value: "2026-11-03 10:15:47", showSeconds: false }); open(control.node);
  control.input.value = "11/05/2026 12:30"; control.input.dispatchEvent(new Event("input", { bubbles: true }));
  document.body.dispatchEvent(new MouseEvent("pointerdown", { bubbles: true })); expect(control.onApply).toHaveBeenCalledExactlyOnceWith("2026-11-05 12:30:47");
  expect(control.input.value).toBe("11/05/2026 12:30"); expect(document.querySelector('.message-popover')).toBeNull();
});

test("datetime main-text clearing or invalid conversion takes precedence over a stale picker draft", () => {
  const control = fixture({ kind: "datetime", value: "2026-11-03 10:15:47", showSeconds: false }); open(control.node);
  control.input.value = "invalid"; document.body.dispatchEvent(new MouseEvent("pointerdown", { bubbles: true }));
  expect(control.onApply).not.toHaveBeenCalled(); expect(control.onInvalid).toHaveBeenLastCalledWith(expect.objectContaining({ kind: "validation", message: "Enter a valid date and time." })); expect(control.input.value).toBe("11/03/2026 10:15");
  open(control.node); control.input.value = ""; document.body.dispatchEvent(new MouseEvent("pointerdown", { bubbles: true })); expect(control.onApply).toHaveBeenCalledExactlyOnceWith(false);
});

test("date textbox leaves expanded state on its actual dialog opener", () => {
  const control = fixture(); expect(control.input.hasAttribute("aria-expanded")).toBe(false); open(control.node);
  expect(control.node.querySelector('[aria-haspopup="dialog"]')?.getAttribute("aria-expanded")).toBe("true");
  key(document.activeElement as HTMLElement, "Escape"); expect(control.input.hasAttribute("aria-expanded")).toBe(false);
});

test("date input has a real native visible label association and retains its error description", () => {
  const control = fixture({ required: true }), label = control.node.querySelector<HTMLLabelElement>("label");
  expect(label).not.toBeNull(); expect(label!.htmlFor).toBe(control.input.id); expect(label!.control).toBe(control.input); expect([...control.input.labels!]).toContain(label);
  expect(control.node.querySelector(`#${control.input.getAttribute("aria-describedby")}`)?.getAttribute("role")).toBe("status");
  expect(label!.querySelector('.field-required-marker')?.getAttribute("aria-hidden")).toBe("true");
});

test("calendar validity callback receives only admitted years at each calendar boundary", () => {
  const isDateValid = vi.fn((date: string) => { expect(Number(date.slice(0, 4))).toBeGreaterThanOrEqual(1); expect(Number(date.slice(0, 4))).toBeLessThanOrEqual(9999); return true; });
  const control = fixture({ value: "0001-01-01", isDateValid }); open(control.node); control.setValue("9999-12-31"); open(control.node);
  expect(isDateValid).toHaveBeenCalled();
});

test("date bounds reject supplied non-string values instead of dropping the restriction", () => {
  service = new PopoverService(document.body);
  const options = { kind: "date" as const, name: "date", label: "Date", value: false as const, locale, popovers: service, onApply: vi.fn(), report: vi.fn() };
  for (const value of [false, null, 0, []]) {
    expect(() => createDateTimeEditor({ ...options, min: value as never })).toThrowError(expect.objectContaining({ kind: "validation", message: "Date/time minimum must be a canonical string." }));
    expect(() => createDateTimeEditor({ ...options, max: value as never })).toThrowError(expect.objectContaining({ kind: "validation", message: "Date/time maximum must be a canonical string." }));
  }
});

test("calendar invalid validity-callback return reaches the report channel", () => {
  const control = fixture({ isDateValid: (() => undefined) as never }); open(control.node);
  expect(control.report).toHaveBeenLastCalledWith(expect.objectContaining({ kind: "validation", message: "Date validity callback must return a boolean." })); expect(document.querySelector('.message-popover')).toBeNull();
});

test("repeated failed datetime opening releases every embedded resize listener", () => {
  const added = vi.spyOn(window, "addEventListener"), removed = vi.spyOn(window, "removeEventListener"), failure = new Error("Validity hook failed");
  const control = fixture({ kind: "datetime", value: "2026-11-03 10:00:00", isDateValid: () => { throw failure; } });
  open(control.node); open(control.node); expect(control.report).toHaveBeenNthCalledWith(1, failure); expect(control.report).toHaveBeenNthCalledWith(2, failure); control.dispose();
  const resizeAdds = added.mock.calls.filter(([type]) => type === "resize").map(([, listener]) => listener);
  const resizeRemoves = removed.mock.calls.filter(([type]) => type === "resize").map(([, listener]) => listener);
  expect(resizeAdds.length).toBeGreaterThan(0); for (const listener of resizeAdds) expect(resizeRemoves).toContain(listener);
  expect(document.querySelector('.message-popover')).toBeNull();
});

test("datetime disposal preserves a close failure while releasing parent and embedded listeners", () => {
  const added = vi.spyOn(window, "addEventListener"), removed = vi.spyOn(window, "removeEventListener"), failure = new Error("Popover close failed");
  const control = fixture({ kind: "datetime", value: "2026-11-03 10:00:00" }), originalOpen = control.service.open.bind(control.service);
  vi.spyOn(control.service, "open").mockImplementation((anchor, content, options) => {
    const handle = originalOpen(anchor, content, options), close = handle.close.bind(handle);
    handle.close = reason => { close(reason); throw failure; }; return handle;
  });
  open(control.node); expect(() => control.dispose()).toThrow(failure); expect(document.querySelector('.message-popover')).toBeNull();
  const resizeRemoves = removed.mock.calls.filter(([type]) => type === "resize").map(([, listener]) => listener);
  for (const [, listener] of added.mock.calls.filter(([type]) => type === "resize")) expect(resizeRemoves).toContain(listener);
  control.input.value = "11/10/2026 10:00"; key(control.input, "Enter"); expect(control.onApply).not.toHaveBeenCalled(); control.dispose();
});

test("range construction failure disposes partial endpoints and validates callbacks and replacement tuples", () => {
  service = new PopoverService(document.body); const added = vi.spyOn(window, "addEventListener"), removed = vi.spyOn(window, "removeEventListener");
  const options = { kind: "date" as const, names: ["start", "end"] as const, labels: ["Start", "End"] as const, value: ["2026-11-03", "invalid"] as const, locale, popovers: service, onApply: vi.fn(), report: vi.fn() };
  for (let attempt = 0; attempt < 2; attempt++) expect(() => createDateTimeRangeEditor(options)).toThrowError(expect.objectContaining({ kind: "server", message: "Invalid represented date value." }));
  const resizeAdds = added.mock.calls.filter(([type]) => type === "resize").map(([, listener]) => listener);
  const resizeRemoves = removed.mock.calls.filter(([type]) => type === "resize").map(([, listener]) => listener);
  for (const listener of resizeAdds) expect(resizeRemoves).toContain(listener);
  expect(() => createDateTimeRangeEditor({ ...options, onApply: undefined as never })).toThrowError(expect.objectContaining({ kind: "validation", message: "Invalid date/time range options." }));
  const applied = vi.fn(), range = createDateTimeRangeEditor({ ...options, value: ["2026-11-03", "2026-11-04"], onApply: applied }); document.body.append(range.node);
  for (const value of [null, false, [], ["2026-11-05"]]) expect(() => range.setValue(value as never)).toThrowError(expect.objectContaining({ kind: "validation", message: "Date/time range value must contain two endpoints." }));
  expect(() => range.setValue(["2026-11-05", "invalid"])).toThrowError(expect.objectContaining({ kind: "server", message: "Invalid represented date value." }));
  expect(range.inputs.map(input => input.value)).toEqual(["11/03/2026", "11/04/2026"]); expect(applied).not.toHaveBeenCalled(); range.dispose();
});

test("range disposal releases both endpoints even when both overlay closes fail", () => {
  service = new PopoverService(document.body); const added = vi.spyOn(window, "addEventListener"), removed = vi.spyOn(window, "removeEventListener"), failure = new Error("Popover close failed");
  const originalOpen = service.open.bind(service);
  vi.spyOn(service, "open").mockImplementation((anchor, content, options) => {
    const handle = originalOpen(anchor, content, options), close = handle.close.bind(handle); handle.close = reason => { close(reason); throw failure; }; return handle;
  });
  const range = createDateTimeRangeEditor({ kind: "date", names: ["start", "end"], labels: ["Start", "End"], value: ["2026-11-03", "2026-11-04"], locale, popovers: service, onApply: vi.fn(), report: vi.fn() }); document.body.append(range.node);
  for (const opener of range.node.querySelectorAll<HTMLButtonElement>('[aria-haspopup="dialog"]')) opener.click();
  expect(() => range.dispose()).toThrowError(expect.objectContaining({ name: "AggregateError", message: "Date/time range cleanup failed.", errors: [failure, failure] })); expect(document.querySelector('.message-popover')).toBeNull();
  const resizeRemoves = removed.mock.calls.filter(([type]) => type === "resize").map(([, listener]) => listener);
  for (const [, listener] of added.mock.calls.filter(([type]) => type === "resize")) expect(resizeRemoves).toContain(listener); range.dispose();
});

/// (partial: reusable range endpoint validation, not linked field declarations)
test("range endpoints emit complete valid tuple and keep required and ordering errors precise", () => {
  service = new PopoverService(document.body); const onApply = vi.fn(), onInvalid = vi.fn();
  const range = createDateTimeRangeEditor({ kind: "date", names: ["start", "end"], labels: ["Start", "End"], value: ["2026-11-03", "2026-11-04"], locale, popovers: service, required: [true, true], onApply, onInvalid, report: vi.fn() });
  document.body.append(range.node); const [start, end] = range.inputs; start.value = "11/05/2026"; key(start, "Enter"); expect(onApply).not.toHaveBeenCalled();
  expect(onInvalid).toHaveBeenLastCalledWith(expect.objectContaining({ kind: "validation", message: "Range start must not be after range end." }));
  end.value = "11/06/2026"; key(end, "Enter"); expect(onApply).toHaveBeenLastCalledWith(["2026-11-05", "2026-11-06"]);
  end.value = ""; key(end, "Enter"); expect(onApply).toHaveBeenCalledTimes(1); expect(onInvalid).toHaveBeenLastCalledWith(expect.objectContaining({ kind: "validation", message: "End is required." })); range.dispose();
});


test.each(["date", "datetime"] as const)("%s unsupported configuration is a local typed error with no mutation or overlay", kind => {
  const abort = new AbortController(), control = fixture({ kind, value: kind === "date" ? "2026-11-03" : "2026-11-03 10:15:00", locale: { ...locale, date_format: "%j" }, signal: abort.signal });
  const message = "Unsupported date/time format directive: %j.";
  expect(control.node.textContent).toContain(message); expect(control.input.getAttribute("aria-invalid")).toBe("true");
  expect(document.getElementById(control.input.getAttribute("aria-describedby")!)?.textContent).toContain(message);
  expect(control.onInvalid).toHaveBeenCalledExactlyOnceWith(expect.objectContaining({ kind: "unsupported", message }));
  expect(control.input.disabled || control.input.readOnly).toBe(true);
  control.input.value = "11/04/2026"; key(control.input, "Enter"); control.input.dispatchEvent(new Event("change", { bubbles: true }));
  expect(control.onApply).not.toHaveBeenCalled(); expect(document.querySelector(".message-popover")).toBeNull();
  control.setValue(kind === "date" ? "2026-11-04" : "2026-11-04 10:15:00"); expect(control.node.textContent).toContain(message);
  expect(control.onApply).not.toHaveBeenCalled(); abort.abort(); control.dispose(); control.dispose(); expect(control.onApply).not.toHaveBeenCalled();
});

test("date control remains usable when only the unused time format is unsupported", () => {
  const control = fixture({ locale: { ...locale, time_format: "%f" } });
  expect(control.input.value).toBe("11/03/2026"); control.input.value = "11/04/2026"; key(control.input, "Enter");
  expect(control.onApply).toHaveBeenCalledExactlyOnceWith("2026-11-04"); expect(control.onInvalid).not.toHaveBeenCalled();
});

test("datetime unsupported time format and required empty phone controls retain local feedback", () => {
  vi.spyOn(window, "innerWidth", "get").mockReturnValue(390);
  const control = fixture({ kind: "datetime", value: false, required: true, locale: { ...locale, time_format: "%f" } });
  expect(control.node.textContent).toContain("Unsupported date/time format directive: %f.");
  expect(control.onInvalid).toHaveBeenCalledExactlyOnceWith(expect.objectContaining({ kind: "unsupported", message: "Unsupported date/time format directive: %f." }));
  expect(control.onApply).not.toHaveBeenCalled(); expect(control.node.querySelector('[aria-haspopup="dialog"]')).toBeNull();
});

test("range endpoints retain visible configuration feedback without applying a pair", () => {
  service = new PopoverService(document.body); const onApply = vi.fn(), onInvalid = vi.fn();
  const range = createDateTimeRangeEditor({ kind: "datetime", names: ["start", "end"], labels: ["Start", "End"], value: ["2026-11-03 10:00:00", "2026-11-03 11:00:00"], locale: { ...locale, date_format: "%j" }, popovers: service, onApply, onInvalid, report: vi.fn() });
  try {
    document.body.append(range.node); expect(range.node.textContent).toContain("Unsupported date/time format directive: %j.");
    expect(range.inputs.every(input => input.disabled || input.readOnly)).toBe(true);
    range.setValue(["2026-11-04 10:00:00", "2026-11-04 11:00:00"]); expect(onApply).not.toHaveBeenCalled();
  } finally { range.dispose(); range.dispose(); }
});


test("unavailable date control reports its typed configuration error when no invalid callback is supplied", () => {
  service = new PopoverService(document.body); const report = vi.fn(), onApply = vi.fn();
  editor = createDateTimeEditor({ kind: "date", name: "date", label: "Date", value: "2026-11-03", locale: { ...locale, date_format: "%j" }, popovers: service, onApply, report });
  document.body.append(editor.node);
  expect(report).toHaveBeenCalledExactlyOnceWith(expect.objectContaining({ kind: "unsupported", message: "Unsupported date/time format directive: %j." }));
  expect(onApply).not.toHaveBeenCalled();
});

test("unsupported date configuration cannot conceal invalid represented values or factory options", () => {
  service = new PopoverService(document.body);
  const options = { kind: "date" as const, name: "date", label: "Date", value: "bad-date", locale: { ...locale, date_format: "%j" }, popovers: service, onApply: vi.fn(), report: vi.fn() };
  expect(() => createDateTimeEditor(options)).toThrowError(expect.objectContaining({ kind: "server", message: "Invalid represented date value." }));
  expect(() => createDateTimeEditor({ ...options, value: "2026-11-03", rounding: -1 })).toThrowError(expect.objectContaining({ kind: "validation", message: "Time rounding must be an integer from 0 to 60 minutes." }));
});
