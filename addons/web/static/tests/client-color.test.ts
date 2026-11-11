/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { afterEach, expect, test, vi } from "vitest";
import { createFieldControl } from "../src/client/fields.js";
import type { Field } from "../src/client/metadata.js";
import { RpcClient, ClientError } from "../src/client/transport.js";

const rpc = new RpcClient(async () => { throw new Error("Unexpected RPC"); }, () => {});

const colorField = (readonly = false, required = false): Field => ({
  name: "color_hex",
  label: "Color Hex",
  kind: "char",
  readonly,
  required,
  relation: null,
  relationField: null,
  selection: null,
  widget: "color",
});

const colorPickerField = (readonly = false, required = false): Field => ({
  name: "color_idx",
  label: "Color Index",
  kind: "integer",
  readonly,
  required,
  relation: null,
  relationField: null,
  selection: null,
  widget: "color_picker",
});

function control(f: Field, value: unknown = "", change = vi.fn()) {
  const handle = createFieldControl({ field: f, value, rpc, context: {}, change, run: job => { void job(); } });
  document.body.append(handle.node);
  return { ...handle, change };
}

afterEach(() => document.body.replaceChildren());

/// (partial: color field renders read swatch with accessible name and handles empty state)
test("color read mode displays swatch with accessible name and title", () => {
  const f = colorField(true);
  const handle = control(f, "#ff0000");

  const swatch = handle.node.querySelector<HTMLElement>(".field-color-swatch")!;
  expect(swatch).not.toBeNull();
  expect(swatch.getAttribute("aria-label")).toBe("Color Hex: #ff0000");
  expect(swatch.title).toBe("#ff0000");
  expect(swatch.style.backgroundColor).toBe("rgb(255, 0, 0)");
  expect(handle.node.querySelectorAll("input[type='color']")).toHaveLength(0);

  handle.dispose();

  // Empty color in read mode
  const handleEmpty = control(f, false);
  const emptySwatch = handleEmpty.node.querySelector<HTMLElement>(".field-color-swatch")!;
  expect(emptySwatch).not.toBeNull();
  expect(emptySwatch.getAttribute("aria-label")).toBe("Color Hex: No color");
  expect(emptySwatch.title).toBe("No color");
  expect(emptySwatch.classList.contains("field-color-empty")).toBe(true);

  handleEmpty.dispose();
});

/// (partial: color edit mode provides native color input and commits one normalised value)
test("color edit mode provides native color input and fires change once on valid hex", () => {
  const handle = control(colorField(false), "#00ff00");

  const input = handle.node.querySelector<HTMLInputElement>("input[type='color']")!;
  expect(input.value).toBe("#00ff00");
  expect(input.getAttribute("aria-label")).toBe("Color Hex");

  input.value = "#123456";
  input.dispatchEvent(new Event("change"));
  expect(handle.change).toHaveBeenCalledTimes(1);
  expect(handle.change).toHaveBeenCalledWith("#123456");
  expect(handle.node.querySelector<HTMLElement>(".field-color-swatch")!.style.backgroundColor).toBe("rgb(18, 52, 86)");

  handle.dispose();
});

/// (partial: color input normalises short hex, uppercase and surrounding whitespace)
test("color value normalisation covers short hex, uppercase and whitespace", () => {
  const short = control(colorField(false), "#abc");
  expect(short.node.querySelector<HTMLInputElement>("input[type='color']")!.value).toBe("#aabbcc");
  short.dispose();
  document.body.replaceChildren();

  const padded = control(colorField(false), "  #FFAA00 ");
  expect(padded.node.querySelector<HTMLInputElement>("input[type='color']")!.value).toBe("#ffaa00");
  padded.dispose();
  document.body.replaceChildren();

  const readonly = control(colorField(true), "#ABC");
  expect(readonly.node.querySelector<HTMLElement>(".field-color-swatch")!.getAttribute("aria-label")).toBe("Color Hex: #aabbcc");
  expect(readonly.node.querySelector(".field-color-readonly")).not.toBeNull();
  readonly.dispose();
});

/// (partial: color edit mode shows an empty value visibly and can be cleared back to empty)
test("color edit mode shows empty value visibly and clears back to empty", () => {
  const empty = control(colorField(false), "");
  const swatch = empty.node.querySelector<HTMLElement>(".field-color-swatch")!;
  const clear = empty.node.querySelector<HTMLButtonElement>(".field-color-clear")!;
  expect(swatch.classList.contains("field-color-empty")).toBe(true);
  expect(swatch.style.backgroundColor).toBe("");
  expect(clear.hidden).toBe(true);
  expect(empty.node.querySelector<HTMLInputElement>("input[type='color']")!.title).toBe("No color");
  empty.dispose();
  document.body.replaceChildren();

  const handle = control(colorField(false), "#ff0000");
  const sw = handle.node.querySelector<HTMLElement>(".field-color-swatch")!;
  const btn = handle.node.querySelector<HTMLButtonElement>(".field-color-clear")!;
  expect(btn.hidden).toBe(false);
  expect(btn.getAttribute("aria-label")).toBe("Clear Color Hex");
  btn.click();
  expect(handle.change).toHaveBeenCalledTimes(1);
  expect(handle.change).toHaveBeenCalledWith("");
  expect(sw.classList.contains("field-color-empty")).toBe(true);
  expect(btn.hidden).toBe(true);
  expect(document.activeElement).toBe(handle.node.querySelector("input[type='color']"));
  handle.dispose();
});

/// (partial: clearing a required color marks the field invalid)
test("color clear on a required field marks it invalid", () => {
  const handle = control(colorField(false, true), "#ff0000");
  handle.node.querySelector<HTMLButtonElement>(".field-color-clear")!.click();
  expect(handle.node.classList.contains("field-invalid")).toBe(true);
  handle.dispose();
});

/// (partial: color widget refuses invalid hex values loudly with ClientError validation)
test("color widget refuses invalid hex format loudly", () => {
  const f = colorField(false);

  expect(() => control(f, "invalid-color")).toThrowError(ClientError);
  expect(() => control(f, "123456")).toThrowError(ClientError);
  expect(() => control(f, "#zzz")).toThrowError(ClientError);
  expect(() => control(f, "#12345")).toThrowError(ClientError);
});

/// (partial: color widget enforces required state when unset)
test("color widget marks invalid when required and empty", () => {
  const f = colorField(false, true);
  const handle = control(f, "");

  expect(handle.node.classList.contains("field-invalid")).toBe(true);
  const error = handle.node.querySelector(".field-error")!;
  expect(error.textContent).toContain("is required");

  handle.dispose();
});

/// (partial: color_picker read mode shows swatch with accessible color name)
test("color_picker read mode shows swatch with palette name as accessible label", () => {
  const f = colorPickerField(true);
  const handle = control(f, 1); // 1 = Red

  const swatch = handle.node.querySelector<HTMLElement>(".field-color-toggler")!;
  expect(swatch).not.toBeNull();
  expect(swatch.getAttribute("aria-label")).toBe("Color Index: Red");
  expect(swatch.title).toBe("Red");
  expect(swatch.classList.contains("field-color-item-1")).toBe(true);
  expect(handle.node.querySelectorAll("button.field-color-toggler")).toHaveLength(0);

  handle.dispose();
});

function openPicker(value: unknown, change = vi.fn()) {
  const handle = control(colorPickerField(false), value, change);
  const toggler = handle.node.querySelector<HTMLButtonElement>("button.field-color-toggler")!;
  const panel = handle.node.querySelector<HTMLElement>(".field-color-palette")!;
  const buttons = Array.from(panel.querySelectorAll<HTMLButtonElement>("button"));
  return { handle, toggler, panel, buttons };
}
const key = (el: Element, k: string) => el.dispatchEvent(new KeyboardEvent("keydown", { key: k, bubbles: true, cancelable: true }));

/// (partial: color_picker edit mode opens palette panel and commits selected index once)
test("color_picker edit mode opens palette and selects color", () => {
  const { handle, toggler, panel, buttons } = openPicker(0);
  expect(toggler.getAttribute("aria-expanded")).toBe("false");
  expect(panel.hidden).toBe(true);
  expect(handle.change).not.toHaveBeenCalled();

  toggler.click();
  expect(panel.hidden).toBe(false);
  expect(toggler.getAttribute("aria-expanded")).toBe("true");
  expect(buttons).toHaveLength(12);
  expect(document.activeElement).toBe(buttons[0]);

  buttons[2]!.click();
  expect(handle.change).toHaveBeenCalledTimes(1);
  expect(handle.change).toHaveBeenCalledWith(2);
  expect(panel.hidden).toBe(true);
  expect(toggler.getAttribute("aria-expanded")).toBe("false");
  expect(toggler.title).toBe("Orange");
  expect(toggler.classList.contains("field-color-item-2")).toBe(true);
  expect(buttons[2]!.classList.contains("active")).toBe(true);
  expect(buttons[2]!.tabIndex).toBe(0);
  expect(buttons[0]!.classList.contains("active")).toBe(false);
  expect(buttons[0]!.tabIndex).toBe(-1);
  expect(document.activeElement).toBe(toggler);
  handle.dispose();
});

/// (partial: color_picker arrows, Home, End move focus and wrap around)
test("color_picker arrows, Home and End move focus and wrap", () => {
  const { handle, toggler, buttons } = openPicker(1);
  toggler.click();
  expect(document.activeElement).toBe(buttons[1]);
  key(buttons[1]!, "ArrowRight");
  expect(document.activeElement).toBe(buttons[2]);
  key(buttons[2]!, "ArrowDown");
  expect(document.activeElement).toBe(buttons[3]);
  key(buttons[3]!, "ArrowLeft");
  expect(document.activeElement).toBe(buttons[2]);
  key(buttons[2]!, "ArrowUp");
  expect(document.activeElement).toBe(buttons[1]);
  key(buttons[1]!, "End");
  expect(document.activeElement).toBe(buttons[11]);
  key(buttons[11]!, "ArrowRight");
  expect(document.activeElement).toBe(buttons[0]);
  key(buttons[0]!, "ArrowLeft");
  expect(document.activeElement).toBe(buttons[11]);
  key(buttons[11]!, "Home");
  expect(document.activeElement).toBe(buttons[0]);
  expect(handle.change).not.toHaveBeenCalled();
  handle.dispose();
});

/// (partial: color_picker Enter and Space select, Escape and Tab close)
test("color_picker Enter and Space select; Escape returns focus to the toggler; Tab closes", () => {
  const { handle, toggler, panel, buttons } = openPicker(1);
  toggler.click();
  key(buttons[1]!, "Enter");
  expect(handle.change).toHaveBeenLastCalledWith(1);
  expect(panel.hidden).toBe(true);
  expect(document.activeElement).toBe(toggler);

  toggler.click();
  key(buttons[4]!, " ");
  expect(handle.change).toHaveBeenLastCalledWith(4);
  expect(handle.change).toHaveBeenCalledTimes(2);

  toggler.click();
  key(buttons[4]!, "Escape");
  expect(panel.hidden).toBe(true);
  expect(toggler.getAttribute("aria-expanded")).toBe("false");
  expect(document.activeElement).toBe(toggler);

  toggler.click();
  key(buttons[4]!, "Tab");
  expect(panel.hidden).toBe(true);
  expect(handle.change).toHaveBeenCalledTimes(2);
  handle.dispose();
});

/// (partial: color_picker closes on outside click and when focus leaves; disposal removes the listener)
test("color_picker closes on outside click and focus leaving, and disposal removes the document listener", () => {
  const outside = document.createElement("button");
  document.body.append(outside);
  const { handle, toggler, panel, buttons } = openPicker(1);

  toggler.click();
  outside.click();
  expect(panel.hidden).toBe(true);

  toggler.click();
  buttons[1]!.dispatchEvent(new FocusEvent("focusout", { bubbles: true, relatedTarget: outside }));
  expect(panel.hidden).toBe(true);

  toggler.click();
  buttons[1]!.dispatchEvent(new FocusEvent("focusout", { bubbles: true, relatedTarget: buttons[2]! }));
  expect(panel.hidden).toBe(false);

  const spy = vi.spyOn(document, "removeEventListener");
  handle.dispose();
  expect(spy.mock.calls.some(([type]) => type === "click")).toBe(true);
  panel.hidden = false;
  outside.click();
  expect(panel.hidden).toBe(false);
  spy.mockRestore();
});

/// (partial: a click on the toggler still reaches other document-level outside-click handlers)
test("color_picker toggler click does not stop propagation", () => {
  const seen = vi.fn();
  document.addEventListener("click", seen);
  const { handle, toggler, buttons } = openPicker(1);
  toggler.click();
  buttons[2]!.click();
  expect(seen).toHaveBeenCalledTimes(2);
  document.removeEventListener("click", seen);
  handle.dispose();
});

/// (partial: a throwing change leaves color_picker UI on the previous value)
test("color_picker keeps previous UI state when change throws", () => {
  const change = vi.fn(() => { throw new Error("rejected"); });
  const { handle, toggler, buttons } = openPicker(1, change);
  toggler.click();
  // DOM listeners report a throw as a window error event instead of propagating it to click().
  const errors: string[] = [];
  const onError = (e: ErrorEvent) => { errors.push(String(e.message)); e.preventDefault(); };
  window.addEventListener("error", onError);
  buttons[3]!.click();
  window.removeEventListener("error", onError);
  expect(change).toHaveBeenCalledTimes(1);
  expect(errors.join(" ")).toContain("rejected");
  expect(toggler.title).toBe("Red");
  expect(toggler.classList.contains("field-color-item-1")).toBe(true);
  expect(buttons[1]!.classList.contains("active")).toBe(true);
  expect(buttons[3]!.classList.contains("active")).toBe(false);
  handle.dispose();
});

/// (partial: color_picker validates integer index range 0 to 11)
test("color_picker refuses non-integer or out-of-bounds indices", () => {
  const f = colorPickerField(false);

  expect(() => control(f, 12)).toThrowError(ClientError);
  expect(() => control(f, -1)).toThrowError(ClientError);
  expect(() => control(f, "abc")).toThrowError(ClientError);
  expect(() => control(f, 1.5)).toThrowError(ClientError);
});

/// (partial: color_picker treats index 0 as a value, like the reference, even when required)
test("color_picker does not treat index 0 as unset when required", () => {
  const handle = control(colorPickerField(false, true), 0);
  expect(handle.node.classList.contains("field-invalid")).toBe(false);
  handle.dispose();
});

// jsdom does not apply the stylesheet, so the `hidden` palette and the swatch geometry are guarded on the rules
// themselves; the live browser check measures the rendered geometry.
const clientCss = readFileSync(resolve(process.cwd(), "..", "addons/web/static/src/client/client.css"), "utf8");
const ruleBody = (selector: string): string => {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const match = new RegExp(`(?:^|\\})\\s*${escaped}\\s*\\{([^}]*)\\}`, "m").exec(clientCss);
  if (!match) throw new Error(`client.css has no rule for ${selector}`);
  return match[1]!;
};

test("color_picker palette stays hidden until opened: the hidden attribute wins over display:grid", () => {
  expect(ruleBody(".field-color-palette")).toMatch(/display:\s*grid/);
  expect(ruleBody(".field-color-palette[hidden]")).toMatch(/display:\s*none/);
});

test("color_picker swatches are fixed circles the global button rule cannot stretch or recolor", () => {
  const swatch = ruleBody(".field-color-picker .field-color-toggler,\n.field-color-picker .field-color-item,\nspan.field-color-toggler");
  for (const declaration of [/width:\s*24px/, /height:\s*24px/, /min-height:\s*0/, /padding:\s*0;/, /aspect-ratio:\s*1/, /border-radius:\s*50%/, /background:\s*var\(--swatch/]) {
    expect(swatch).toMatch(declaration);
  }
  expect(ruleBody(".field-color-picker .field-color-toggler:hover:not(:disabled),\n.field-color-picker .field-color-item:hover")).toMatch(/background:\s*var\(--swatch/);
  for (let index = 1; index <= 11; index++) expect(ruleBody(`.field-color-item-${index}`)).toContain(`--swatch: var(--palette-${index})`);
});

test("boolean_favorite renders as a bare symbol, not a full-width bordered button", () => {
  const body = ruleBody(".field-shell .field-boolean-button");
  for (const declaration of [/justify-self:\s*start/, /width:\s*auto/, /min-height:\s*0/, /border:\s*0/, /background:\s*transparent/]) {
    expect(body).toMatch(declaration);
  }
});
