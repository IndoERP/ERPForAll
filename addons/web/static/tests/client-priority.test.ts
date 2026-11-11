/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { createFieldControl } from "../src/client/fields.js";
import type { Field } from "../src/client/metadata.js";
import { RpcClient, ClientError } from "../src/client/transport.js";

const rpc = new RpcClient(async () => { throw new Error("Unexpected RPC"); }, () => {});

const priorityField = (readonly = false, required = false, options: Record<string, unknown> = {}): Field => ({
  name: "priority",
  label: "Priority",
  kind: "selection",
  readonly,
  required,
  relation: null,
  relationField: null,
  selection: [
    ["0", "Normal"],
    ["1", "Low"],
    ["2", "High"],
    ["3", "Very High"],
  ],
  widget: "priority",
  options,
});

function control(f: Field, value: unknown = "0", change = vi.fn()) {
  const handle = createFieldControl({ field: f, value, rpc, context: {}, change, run: job => { void job(); } });
  document.body.append(handle.node);
  return { ...handle, change };
}

afterEach(() => document.body.replaceChildren());

/// (partial: priority read mode shows filled and empty stars with accessible labels)
test("priority read mode displays filled and empty stars with option labels as accessible names", () => {
  const f = priorityField(true);
  const handle = control(f, "2");

  const group = handle.node.querySelector('[role="radiogroup"]')!;
  expect(group).not.toBeNull();
  expect(group.getAttribute("aria-label")).toBe("Priority");

  const stars = handle.node.querySelectorAll('.o_priority_star');
  // 4 choices -> 3 stars (choices.slice(1))
  expect(stars).toHaveLength(3);

  // For value "2": star 0 ("Low") and star 1 ("High") filled; star 2 ("Very High") empty
  expect(stars[0]!.textContent).toBe("★");
  expect(stars[0]!.getAttribute("aria-label")).toBe("Low");
  expect(stars[0]!.getAttribute("aria-checked")).toBe("false");
  expect(stars[0]!.classList.contains("oi-filled")).toBe(true);

  expect(stars[1]!.textContent).toBe("★");
  expect(stars[1]!.getAttribute("aria-label")).toBe("High");
  expect(stars[1]!.getAttribute("aria-checked")).toBe("true");
  expect(stars[1]!.classList.contains("oi-filled")).toBe(true);

  expect(stars[2]!.textContent).toBe("☆");
  expect(stars[2]!.getAttribute("aria-label")).toBe("Very High");
  expect(stars[2]!.getAttribute("aria-checked")).toBe("false");
  expect(stars[2]!.classList.contains("oi-filled")).toBe(false);

  // Non-editable spans in read mode
  expect(handle.node.querySelectorAll("button.o_priority_star")).toHaveLength(0);

  handle.dispose();
});

/// (partial: priority click sets value and clicking current star clears to first option)
test("priority click sets value and clicking the currently selected star clears to the first option", () => {
  const f = priorityField(false);
  const handle = control(f, "0");

  const buttons = handle.node.querySelectorAll<HTMLButtonElement>("button.o_priority_star");
  expect(buttons).toHaveLength(3);

  // Click Star 2 ("High") -> sets "2"
  buttons[1]!.click();
  expect(handle.change).toHaveBeenCalledWith("2");

  handle.dispose();

  // If already at "2", clicking Star 2 ("High") clears to "0"
  const handleSelected = control(f, "2");
  const selectedButtons = handleSelected.node.querySelectorAll<HTMLButtonElement>("button.o_priority_star");
  selectedButtons[1]!.click();
  expect(handleSelected.change).toHaveBeenCalledWith("0");

  handleSelected.dispose();
});

/// (partial: priority keyboard navigation updates selection and clears to first option)
test("priority keyboard navigation updates selection using arrow keys", () => {
  const f = priorityField(false);
  const handle = control(f, "1");

  const buttons = handle.node.querySelectorAll<HTMLButtonElement>("button.o_priority_star");

  // ArrowRight on Star 1 (index 0) moves to Star 2 (index 1) and selects "2"
  buttons[0]!.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowRight", bubbles: true }));
  expect(handle.change).toHaveBeenCalledWith("2");

  // ArrowLeft on Star 1 (index 0) clears to first option "0"
  buttons[0]!.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowLeft", bubbles: true }));
  expect(handle.change).toHaveBeenCalledWith("0");

  handle.dispose();
});

/// (partial: on a required field, clicking the current star still commits the first choice)
test("priority on a required field clears to the first choice, which is a real value", () => {
  const f = priorityField(false, true); // required
  const handle = control(f, "2");

  const buttons = handle.node.querySelectorAll<HTMLButtonElement>("button.o_priority_star");

  buttons[1]!.click();
  expect(handle.change).toHaveBeenCalledWith("0");
  expect(handle.node.classList.contains("field-invalid")).toBe(false);
  expect(buttons[1]!.getAttribute("aria-checked")).toBe("false");

  buttons[0]!.click();
  expect(handle.change).toHaveBeenLastCalledWith("1");

  handle.dispose();
});

/// (partial: ArrowRight on the last star keeps the value)
test("priority ArrowRight on the last star does not clear the selection", () => {
  const handle = control(priorityField(false), "3");
  const buttons = handle.node.querySelectorAll<HTMLButtonElement>("button.o_priority_star");
  buttons[2]!.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowRight", bubbles: true }));
  expect(handle.change).not.toHaveBeenCalled();
  expect(buttons[2]!.getAttribute("aria-checked")).toBe("true");
  handle.dispose();
});

/// (partial: tooltips name the level a click sets, or the current level when readonly)
test("priority tooltips name the level a click would set", () => {
  const edit = control(priorityField(false), "2");
  const buttons = edit.node.querySelectorAll<HTMLButtonElement>("button.o_priority_star");
  expect(buttons[0]!.title).toBe("Set Priority: Low");
  expect(buttons[1]!.title).toBe("Set Priority: Normal"); // the current star clears to the first choice
  expect(buttons[2]!.title).toBe("Set Priority: Very High");
  buttons[2]!.click();
  expect(buttons[1]!.title).toBe("Set Priority: High");
  expect(buttons[2]!.title).toBe("Set Priority: Normal");
  edit.dispose();

  const read = control(priorityField(true), "2");
  const spans = read.node.querySelectorAll<HTMLElement>("span.o_priority_star");
  expect([...spans].map(span => span.title)).toEqual(["High", "High", "High"]);
  read.dispose();
});

/// (partial: priority widget rejects placeholder options and invalid field configurations)
test("priority widget rejects placeholder options and non-selection fields", () => {
  expect(() => {
    createFieldControl({
      field: priorityField(false, false, { placeholder: "None" }),
      value: "0",
      rpc,
      context: {},
      change: vi.fn(),
      run: job => { void job(); },
    });
  }).toThrow(ClientError);

  expect(() => {
    createFieldControl({
      field: { ...priorityField(false), kind: "char" },
      value: "0",
      rpc,
      context: {},
      change: vi.fn(),
      run: job => { void job(); },
    });
  }).toThrow(ClientError);
});
