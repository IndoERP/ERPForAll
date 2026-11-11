/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { createTimeEditor, type TimeEditorHandle } from "../src/client/time-editor.js";
import { PopoverService } from "../src/client/popover.js";

const locale = { lang: "en_US", tz: "UTC", date_format: "%m/%d/%Y", time_format: "%H:%M:%S" };
let service: PopoverService | undefined, editor: TimeEditorHandle | undefined;
afterEach(() => { editor?.dispose(); service?.dispose(); editor = undefined; service = undefined; document.body.replaceChildren(); vi.restoreAllMocks(); });
function fixture(options: Partial<Parameters<typeof createTimeEditor>[0]> = {}) {
  service = new PopoverService(document.body); const onApply = vi.fn(), onInvalid = vi.fn(), report = vi.fn();
  editor = createTimeEditor({ name: "time", label: "Time", value: "10:00:00", locale, popovers: service, onApply, onInvalid, report, ...options });
  document.body.append(editor.node); return { ...editor, onApply, onInvalid, report, service };
}
function key(input: HTMLElement, value: string): void { input.dispatchEvent(new KeyboardEvent("keydown", { key: value, bubbles: true, cancelable: true })); }

/// (partial: desktop pointer, typed validity and keyboard routes; phone rendering is separate)
test("time suggestions preview with arrows and accept once with Enter or Tab", () => {
  const { input, onApply } = fixture(); expect(input.value).toBe("10:00"); input.focus();
  input.dispatchEvent(new Event("input", { bubbles: true }));
  expect(document.querySelector('[role="listbox"]')).not.toBeNull(); key(input, "ArrowDown");
  const preview = input.value; expect(preview).not.toBe("10:00"); expect(onApply).not.toHaveBeenCalled(); key(input, "Enter");
  expect(onApply).toHaveBeenCalledTimes(1); expect(document.querySelector('.message-popover')).toBeNull();
  input.focus(); input.dispatchEvent(new Event("input", { bubbles: true })); key(input, "ArrowDown"); key(input, "Tab"); expect(onApply).toHaveBeenCalledTimes(2);
});

test("time invalid text retains visible invalid state and precise callback without a change", () => {
  const { input, onApply, onInvalid } = fixture(); input.value = "25:67"; input.dispatchEvent(new Event("input", { bubbles: true }));
  expect(input.getAttribute("aria-invalid")).toBe("true"); input.dispatchEvent(new Event("change", { bubbles: true }));
  expect(onApply).not.toHaveBeenCalled(); expect(onInvalid).toHaveBeenCalledWith(expect.objectContaining({ kind: "validation", message: "Enter a valid time." }));
  expect(document.querySelector('.message-popover')).toBeNull();
  expect(document.getElementById(input.getAttribute("aria-describedby")!)?.hidden).toBe(false);
  expect(input.value).toBe("25:67");
});

test("time typed Enter parses, Escape restores, clear applies false, readonly cannot mutate", () => {
  const { input, onApply } = fixture(); input.value = "11:23"; key(input, "Enter"); expect(onApply).toHaveBeenLastCalledWith("11:23:00");
  input.value = "12:00"; key(input, "Escape"); expect(input.value).toBe("11:23"); expect(onApply).toHaveBeenCalledTimes(1);
  input.value = ""; input.dispatchEvent(new Event("change", { bubbles: true })); expect(onApply).toHaveBeenLastCalledWith(false);
  editor!.dispose(); const readonly = fixture({ readonly: true }); readonly.input.value = "13:00"; key(readonly.input, "Enter"); expect(readonly.onApply).not.toHaveBeenCalled();
});

/// (partial: phone native time route)
test("time uses numeric native phone input and disposes open overlays on abort", () => {
  vi.spyOn(window, "innerWidth", "get").mockReturnValue(390); const abort = new AbortController();
  const { input, onApply } = fixture({ signal: abort.signal, showSeconds: true }); expect(input.type).toBe("time"); expect(input.value).toBe("10:00:00");
  input.value = "10:23:45"; input.dispatchEvent(new Event("change", { bubbles: true })); expect(onApply).toHaveBeenLastCalledWith("10:23:45");
  abort.abort(); input.value = "12:00:00"; key(input, "Enter"); expect(onApply).toHaveBeenCalledTimes(1);
});

test("time suggestion pointer, readonly and callback reentrancy respect lifetime", () => {
  const applied = vi.fn(() => { editor?.dispose(); }); const { input, report } = fixture({ onApply: applied }); input.focus(); input.dispatchEvent(new Event("input", { bubbles: true }));
  const option = document.querySelector<HTMLElement>('[role="option"]'); expect(option).not.toBeNull(); option!.click(); expect(document.querySelector('.message-popover')).toBeNull(); expect(report).not.toHaveBeenCalled();
  expect(applied).toHaveBeenCalledTimes(1);
});

test("time Escape and owner navigation cancel preview without applying", () => {
  const { input, service, onApply } = fixture(); input.focus(); input.dispatchEvent(new Event("input", { bubbles: true })); key(input, "ArrowDown"); key(input, "Escape");
  expect(input.value).toBe("10:00"); expect(onApply).not.toHaveBeenCalled(); input.dispatchEvent(new Event("input", { bubbles: true })); key(input, "ArrowDown"); service.navigation(); expect(onApply).not.toHaveBeenCalled();
});

test("time Tab accepts a typed active suggestion and native badInput never becomes a cleared value", () => {
  const control = fixture({ rounding: 0 }); control.input.focus(); control.input.value = "10:30"; control.input.dispatchEvent(new Event("input", { bubbles: true })); key(control.input, "Tab");
  expect(control.onApply).toHaveBeenLastCalledWith("10:30:00");
  vi.spyOn(window, "innerWidth", "get").mockReturnValue(390); window.dispatchEvent(new Event("resize"));
  vi.spyOn(control.input, "validity", "get").mockReturnValue({ badInput: true } as ValidityState); control.input.value = ""; control.input.dispatchEvent(new Event("change", { bubbles: true }));
  expect(control.onApply).toHaveBeenCalledTimes(1); expect(control.onInvalid).toHaveBeenLastCalledWith(expect.objectContaining({ kind: "validation", message: "Enter a valid time." }));
});

test("time suggestion pointer preserves represented seconds when seconds are hidden", () => {
  const control = fixture({ showSeconds: false, rounding: 0, value: "10:00:47" }); control.input.focus();
  document.querySelector<HTMLButtonElement>('[role="option"]')!.click(); expect(control.onApply).toHaveBeenLastCalledWith("00:00:47");
});

test("time combobox uses a real native label and keeps the validation description separate", () => {
  const control = fixture(), label = control.node.querySelector<HTMLLabelElement>("label");
  expect(label).not.toBeNull(); expect(label!.htmlFor).toBe(control.input.id); expect(label!.control).toBe(control.input); expect([...control.input.labels!]).toContain(label);
  expect(control.node.querySelector(`#${control.input.getAttribute("aria-describedby")}`)?.getAttribute("role")).toBe("status");
});

test("time suggestion scroll container accepts keyboard focus and retains Arrow/Enter selection", () => {
  const control = fixture(); control.input.focus(); const list = document.querySelector<HTMLElement>('.time-suggestions')!;
  expect(list.tabIndex).toBe(0); expect(list.getAttribute("role")).toBe("listbox"); expect(control.input.getAttribute("aria-controls")).toBe(list.id);
  list.focus(); key(list, "ArrowDown"); expect(control.onApply).not.toHaveBeenCalled(); expect(list.getAttribute("aria-activedescendant")).not.toBeNull(); key(list, "Enter");
  expect(control.onApply).toHaveBeenLastCalledWith("10:15:00"); expect(document.activeElement).toBe(control.input); expect(document.querySelector('.message-popover')).toBeNull();
});

test("time list Escape cancels and list Tab preserves the combobox as native departure origin", () => {
  const control = fixture(); control.input.focus(); let list = document.querySelector<HTMLElement>('.time-suggestions')!; list.focus(); key(list, "ArrowDown"); key(list, "Escape");
  expect(control.onApply).not.toHaveBeenCalled(); expect(control.input.value).toBe("10:00"); expect(document.activeElement).toBe(control.input); expect(document.querySelector('.message-popover')).toBeNull();
  key(control.input, "ArrowDown"); list = document.querySelector<HTMLElement>('.time-suggestions')!; list.focus();
  const tab = new KeyboardEvent("keydown", { key: "Tab", bubbles: true, cancelable: true }); list.dispatchEvent(tab);
  expect(tab.defaultPrevented).toBe(false); expect(control.onApply).toHaveBeenLastCalledWith("10:15:00"); expect(document.activeElement).toBe(control.input); expect(document.querySelector('.message-popover')).toBeNull();
});

test("time outside departure applies the active suggestion once before any native change", () => {
  const control = fixture(); control.input.focus(); key(control.input, "ArrowDown"); document.body.dispatchEvent(new MouseEvent("pointerdown", { bubbles: true }));
  expect(control.onApply).toHaveBeenLastCalledWith("10:15:00"); control.input.dispatchEvent(new Event("change", { bubbles: true })); expect(control.onApply).toHaveBeenCalledTimes(1);
});

test("time ArrowUp without an active suggestion starts at the last suggestion", () => {
  const control = fixture({ value: "10:07:00", rounding: 0 }); control.input.focus(); key(control.input, "ArrowUp");
  expect(control.input.value).toBe("23:45"); expect(control.onApply).not.toHaveBeenCalled(); key(control.input, "Enter"); expect(control.onApply).toHaveBeenLastCalledWith("23:45:00");
});

test("time disposal preserves a close failure while releasing all owned listeners", () => {
  const added = vi.spyOn(window, "addEventListener"), removed = vi.spyOn(window, "removeEventListener"), failure = new Error("Popover close failed");
  const control = fixture(), originalOpen = control.service.open.bind(control.service);
  vi.spyOn(control.service, "open").mockImplementation((anchor, content, options) => {
    const handle = originalOpen(anchor, content, options), close = handle.close.bind(handle);
    handle.close = reason => { close(reason); throw failure; }; return handle;
  });
  control.input.focus(); expect(() => control.dispose()).toThrow(failure); expect(document.querySelector('.message-popover')).toBeNull();
  const resizeRemoves = removed.mock.calls.filter(([type]) => type === "resize").map(([, listener]) => listener);
  for (const [, listener] of added.mock.calls.filter(([type]) => type === "resize")) expect(resizeRemoves).toContain(listener);
  control.input.value = "12:30"; key(control.input, "Enter"); expect(control.onApply).not.toHaveBeenCalled(); control.dispose();
});

test("time control accepts typed and suggested values, refuses invalid change, and switches to the native phone route", () => {
  const control = fixture({ showSeconds: true, rounding: 0 });
  control.input.focus(); const option = document.querySelector<HTMLButtonElement>('[role="option"]')!; option.click();
  expect(control.onApply).toHaveBeenLastCalledWith("00:00:00"); expect(document.querySelector('.message-popover')).toBeNull();
  control.input.value = "11:22:33"; key(control.input, "Enter"); expect(control.onApply).toHaveBeenLastCalledWith("11:22:33");
  const applied = control.onApply.mock.calls.length; control.input.value = "25:75"; control.input.dispatchEvent(new Event("input", { bubbles: true })); control.input.dispatchEvent(new Event("change", { bubbles: true }));
  expect(control.input.getAttribute("aria-invalid")).toBe("true"); expect(control.onInvalid).toHaveBeenLastCalledWith(expect.objectContaining({ kind: "validation", message: "Enter a valid time." })); expect(control.onApply).toHaveBeenCalledTimes(applied);
  key(control.input, "Escape"); expect(control.input.value).toBe("11:22:33"); control.input.focus(); key(control.input, "ArrowDown");
  expect(control.onApply).toHaveBeenCalledTimes(applied); key(control.input, "Tab"); expect(control.onApply).toHaveBeenCalledTimes(applied + 1);
  const width = vi.spyOn(window, "innerWidth", "get").mockReturnValue(390); window.dispatchEvent(new Event("resize")); expect(control.input.type).toBe("time");
  control.input.value = "12:05:06"; control.input.dispatchEvent(new Event("change", { bubbles: true })); expect(control.onApply).toHaveBeenLastCalledWith("12:05:06"); expect(document.querySelector('.message-popover')).toBeNull();
  editor!.dispose(); width.mockReturnValue(1024);
  const rounded = fixture({ rounding: 15, showSeconds: false, value: "10:00:42" }); rounded.input.value = "10:08"; rounded.input.dispatchEvent(new Event("change", { bubbles: true })); expect(rounded.onApply).toHaveBeenLastCalledWith("10:15:42");
});


test("unsupported time configuration renders typed local feedback and preserves lifetime without changes", () => {
  const abort = new AbortController(), control = fixture({ locale: { ...locale, time_format: "%f" }, signal: abort.signal });
  const message = "Unsupported date/time format directive: %f.";
  expect(control.node.textContent).toContain(message); expect(control.input.getAttribute("aria-invalid")).toBe("true");
  expect(document.getElementById(control.input.getAttribute("aria-describedby")!)?.textContent).toContain(message);
  expect(control.onInvalid).toHaveBeenCalledExactlyOnceWith(expect.objectContaining({ kind: "unsupported", message }));
  expect(control.input.disabled || control.input.readOnly).toBe(true);
  control.input.value = "11:00"; key(control.input, "Enter"); control.input.dispatchEvent(new Event("change", { bubbles: true }));
  expect(control.onApply).not.toHaveBeenCalled(); expect(document.querySelector(".message-popover")).toBeNull();
  control.setValue("12:00:00"); expect(control.node.textContent).toContain(message); expect(control.onApply).not.toHaveBeenCalled();
  abort.abort(); control.dispose(); control.dispose(); expect(control.onApply).not.toHaveBeenCalled();
});

test("time control remains usable with unsupported unused date format", () => {
  const control = fixture({ locale: { ...locale, date_format: "%j" } });
  expect(control.input.value).toBe("10:00"); control.input.value = "11:20"; key(control.input, "Enter");
  expect(control.onApply).toHaveBeenCalledExactlyOnceWith("11:20:00"); expect(control.onInvalid).not.toHaveBeenCalled();
});


test("unavailable time control reports configuration errors when no invalid callback is supplied", () => {
  service = new PopoverService(document.body); const report = vi.fn(), onApply = vi.fn();
  editor = createTimeEditor({ name: "time", label: "Time", value: "10:00:00", locale: { ...locale, time_format: "%f" }, popovers: service, onApply, report });
  document.body.append(editor.node);
  expect(report).toHaveBeenCalledExactlyOnceWith(expect.objectContaining({ kind: "unsupported", message: "Unsupported date/time format directive: %f." }));
  expect(onApply).not.toHaveBeenCalled();
});

test("unsupported time configuration cannot conceal invalid represented values or rounding options", () => {
  service = new PopoverService(document.body);
  const options = { name: "time", label: "Time", value: "bad-time", locale: { ...locale, time_format: "%f" }, popovers: service, onApply: vi.fn(), report: vi.fn() };
  expect(() => createTimeEditor(options)).toThrowError(expect.objectContaining({ kind: "server", message: "Invalid represented time value." }));
  expect(() => createTimeEditor({ ...options, value: "10:00:00", rounding: -1 })).toThrowError(expect.objectContaining({ kind: "validation", message: "Time rounding must be an integer from 0 to 60 minutes." }));
});
