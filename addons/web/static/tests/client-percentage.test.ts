/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { createFieldControl } from "../src/client/fields.js";
import type { Field } from "../src/client/metadata.js";
import { fractionFromPercent, percentFromFraction, type NumericLocale } from "../src/client/numeric-values.js";
import { RpcClient } from "../src/client/transport.js";

const rpc = new RpcClient(async () => { throw new Error("Unexpected RPC"); }, () => {});
const english: NumericLocale = { decimal_point: ".", thousands_sep: ",", grouping: [3, 0] };
const german: NumericLocale = { decimal_point: ",", thousands_sep: ".", grouping: [3, 0] };
const field = (kind: "float" | "integer", readonly = false, options: Record<string, unknown> = {}): Field & { widget: string; options: Record<string, unknown> } => ({
  name: "rate", label: "Rate", kind, readonly, required: false, relation: null, relationField: null, selection: null, widget: "percentage", options,
});
function control(f: Field, value: unknown, numericLocale: NumericLocale = english, change = vi.fn()) {
  const handle = createFieldControl({ field: f, value, numericLocale, rpc, context: {}, change, run: job => { void job(); } });
  document.body.append(handle.node); return { ...handle, change };
}
function type(handle: ReturnType<typeof control>, text: string) {
  const input = handle.node.querySelector("input")!; input.value = text;
  input.dispatchEvent(new Event("input")); input.dispatchEvent(new Event("change")); return input;
}
afterEach(() => document.body.replaceChildren());

/// (partial: percentage only; conversion is decimal-exact in both directions)
test("percentage conversion is exact and inverse", () => {
  expect(percentFromFraction(0.07)).toBe(7);
  expect(percentFromFraction(0.29)).toBe(29);
  expect(percentFromFraction(1.15)).toBe(115);
  expect(fractionFromPercent(7)).toBe(0.07);
  expect(fractionFromPercent(12.5)).toBe(0.125);
  for (const fraction of [0, 0.01, 0.07, 0.1, 0.29, 0.5, 1, 2.5, -0.33]) expect(fractionFromPercent(percentFromFraction(fraction))).toBe(fraction);
});

/// (partial: percentage only; read mode shows fraction x100 with the percent symbol)
test("read mode shows the percent with a symbol and honours no_symbol, digits and locale", () => {
  expect(control(field("float", true), 0.075).node.querySelector(".field-read-value")?.textContent).toBe("7.5%");
  document.body.replaceChildren();
  expect(control(field("float", true, { no_symbol: true }), 0.075).node.querySelector(".field-read-value")?.textContent).toBe("7.5");
  document.body.replaceChildren();
  expect(control(field("float", true, { digits: [16, 2] }), 0.075).node.querySelector(".field-read-value")?.textContent).toBe("7.50%");
  document.body.replaceChildren();
  expect(control(field("float", true), 12.345, german).node.querySelector(".field-read-value")?.textContent).toBe("1.234,5%");
  document.body.replaceChildren();
  expect(control(field("integer", true), 2).node.querySelector(".field-read-value")?.textContent).toBe("200%");
  document.body.replaceChildren();
  expect(control(field("float", true), false).node.querySelector(".field-read-value")?.textContent).toBe("");
});

/// (partial: percentage only; edit takes the percent and stores the fraction)
test("edit shows the percent number and commits the fraction, symbol optional", () => {
  const handle = control(field("float"), 0.075);
  const input = handle.node.querySelector("input")!;
  expect(input.value).toBe("7.5");
  type(handle, "12.5"); expect(handle.change).toHaveBeenLastCalledWith(0.125);
  type(handle, "30 %"); expect(handle.change).toHaveBeenLastCalledWith(0.3);
  type(handle, "7%"); expect(handle.change).toHaveBeenLastCalledWith(0.07);
  type(handle, ""); expect(handle.change).toHaveBeenLastCalledWith(false);
  expect(handle.change).toHaveBeenCalledTimes(4);
});

/// (partial: percentage only; edit parses the locale decimal separator)
test("edit parses the locale decimal separator", () => {
  const handle = control(field("float"), 0.075, german);
  expect(handle.node.querySelector("input")!.value).toBe("7,5");
  type(handle, "12,5"); expect(handle.change).toHaveBeenLastCalledWith(0.125);
});

/// (partial: percentage only; invalid input is refused with a message and nothing is committed)
test("invalid percent input shows a message and does not change the value", () => {
  const handle = control(field("float"), 0.5);
  type(handle, "abc%");
  expect(handle.change).not.toHaveBeenCalled();
  expect(handle.node.querySelector("[role=alert], .field-error, [aria-invalid=true]")).not.toBeNull();
  type(handle, "1e3"); expect(handle.change).not.toHaveBeenCalled();
  type(handle, "%"); expect(handle.change).not.toHaveBeenCalled();
});

/// (partial: percentage only; integer fields refuse a percent that is not a whole stored value)
test("integer percentage refuses a non-integer stored value", () => {
  const handle = control(field("integer"), 2);
  expect(handle.node.querySelector("input")!.value).toBe("200");
  type(handle, "250"); expect(handle.change).not.toHaveBeenCalled();
  type(handle, "300"); expect(handle.change).toHaveBeenLastCalledWith(3);
});

/// (partial: percentage only; a bad stored value fails loudly)
test("percentage refuses a non-numeric stored value", () => {
  expect(() => control(field("float"), "7")).toThrow(/numeric|number/i);
});

/// (partial: percentage only; a missing numeric locale fails loudly instead of showing the raw fraction)
test("percentage without a numeric locale is refused", () => {
  expect(() => createFieldControl({ field: field("float", true), value: 0.075, rpc, context: {}, change: vi.fn(), run: job => { void job(); } })).toThrow("Percentage fields need the numeric locale.");
});

/// (partial: percentage only; a stored value whose percent is out of range is an inline error, not a thrown render)
test("an out-of-range stored value renders as an invalid field", () => {
  const handle = control(field("float", true), 1e14);
  expect(handle.node.querySelector("[aria-invalid=true], .field-invalid")).not.toBeNull();
  expect(handle.node.textContent).toContain("Percentage is outside the supported range.");
});

/// (partial: percentage only; a lone percent symbol is refused and never clears the stored value)
test("a lone percent symbol is refused with a message", () => {
  const handle = control(field("float"), 0.5);
  type(handle, "%"); expect(handle.change).not.toHaveBeenCalled();
  expect(handle.node.textContent).toContain("Enter a percentage before the % symbol.");
});
