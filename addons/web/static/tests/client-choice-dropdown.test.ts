/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { createChoiceDropdown } from "../src/client/choice-dropdown.js";
afterEach(() => { document.body.replaceChildren(); vi.restoreAllMocks(); });
const choices = [{ value: "active", label: "Active" }, { value: "archived", label: "Archived" }];
test("shared choice synchronizes checked state without replacing focus and disposes listeners", () => {
  const change = vi.fn();
  const control = createChoiceDropdown({ label: "Records", choices, value: "active", onChange: change });
  document.body.append(control.element);
  control.trigger.focus();
  control.trigger.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));
  const items = [...control.element.querySelectorAll<HTMLButtonElement>('[role="menuitemradio"]')];
  expect(document.activeElement).toBe(items[0]);
  items[0]!.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));
  expect(document.activeElement).toBe(items[1]);
  items[1]!.click();
  expect(change).toHaveBeenCalledWith("archived");
  expect(control.trigger.textContent).toContain("Archived");
  expect(document.activeElement).toBe(control.trigger);
  control.setValue("active");
  expect(document.activeElement).toBe(control.trigger);
  expect(items.map(item => item.getAttribute("aria-checked"))).toEqual(["true", "false"]);
  control.trigger.click();
  control.setDisabled(true);
  expect(control.trigger.getAttribute("aria-expanded")).toBe("false");
  items[1]!.dispatchEvent(new Event("click"));
  expect(change).toHaveBeenCalledTimes(1);
  control.setDisabled(false);
  control.trigger.click();
  control.dispose(); control.dispose();
  expect(control.trigger.getAttribute("aria-expanded")).toBe("false");
  control.trigger.click(); items[1]!.dispatchEvent(new Event("click"));
  expect(control.trigger.getAttribute("aria-expanded")).toBe("false");
  expect(change).toHaveBeenCalledTimes(1);
});
test("choice Escape and outside dismissal share menu behavior", () => {
  const control = createChoiceDropdown({ label: "Records", choices, value: "active", onChange: () => {} });
  document.body.append(control.element);
  control.trigger.click();
  control.element.querySelector('[role="menu"]')!.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
  expect(document.activeElement).toBe(control.trigger);
  control.trigger.click(); document.body.dispatchEvent(new Event("pointerdown", { bubbles: true }));
  expect(control.trigger.getAttribute("aria-expanded")).toBe("false");
  control.dispose();
});
test("unknown choices fail explicitly and labels are text", () => {
  const control = createChoiceDropdown({ label: "Records", choices: [{ value: "a", label: "<img src=x>" }], value: "a", onChange: () => {} });
  expect(control.element.querySelector("img")).toBeNull();
  expect(() => control.setValue("missing")).toThrowError(expect.objectContaining({ kind: "validation", message: "Unknown choice for Records: missing." }));
  control.dispose();
  expect(() => createChoiceDropdown({ label: "Records", choices: [...choices, choices[0]!], value: "active", onChange: () => {} })).toThrowError(expect.objectContaining({ kind: "validation", message: "Duplicate choice for Records: active." }));
});

test("choice admission bounds entries before constructing menus", () => {
  expect(() => createChoiceDropdown({ label: "Records", choices: Array.from({length:1025}, (_,i) => ({value:String(i),label:String(i)})), value: "0", onChange: () => {} })).toThrowError(expect.objectContaining({ kind: "validation", message: "Choices exceed 1024 entries or 262144 text bytes." }));
});

// Review S4: the accessible name carries the current value, and follows it.
test("the trigger's accessible name carries the current value", () => {
  const control = createChoiceDropdown({ label: "Records", choices, value: "active", onChange: () => {} });
  expect(control.trigger.getAttribute("aria-label")).toBe("Records: Active");
  control.setValue("archived");
  expect(control.trigger.getAttribute("aria-label")).toBe("Records: Archived");
});
