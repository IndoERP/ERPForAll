/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test } from "vitest";
import { createListTabs, decodeSelectionCounts } from "../src/client/list-tabs.js";
import type { Field } from "../src/client/metadata.js";
const field: Field = { name: "priority", kind: "selection", label: "Priority", required: false, readonly: false,
  selection: [["normal", "Normal"], ["high", "High"]], relation: null, relationField: null };
afterEach(() => document.body.replaceChildren());

/// (partial: strict grouped-count decoding and nullable/missing states)
test("selection tab counts use actual groups, include unset in All and show absent declared choices as zero", () => {
  const counts = decodeSelectionCounts([{ priority: "normal", __count: 3 }, { priority: false, __count: 2 }], field);
  expect(counts.total).toBe(5);
  expect(counts.values.get("normal")).toBe(3);
  expect(counts.values.get("high")).toBe(0);
  expect(counts.unset).toBe(2);
});

/// (partial: malformed count responses fail instead of manufacturing totals)
test("selection counts refuse unknown, duplicate, null, negative, unsafe and required-unset groups", () => {
  for (const [raw, message] of [
    [[{ priority: "other", __count: 1 }], "Unknown selection tab group: other."],
    [[{ priority: "normal", __count: 1 }, { priority: "normal", __count: 2 }], "Duplicate selection tab group."],
    [[{ priority: null, __count: 1 }], "Invalid selection tab group value."],
    [[{ priority: "normal", __count: -1 }], "Invalid selection tab count."],
    [[{ priority: "normal", __count: Number.MAX_SAFE_INTEGER + 1 }], "Invalid selection tab count."],
    [[{ priority: "normal", __count: Number.MAX_SAFE_INTEGER }, { priority: false, __count: 1 }], "Selection tab total exceeds the browser safe integer range."],
  ] as const) {
    try { decodeSelectionCounts(raw, field); throw new Error("Expected count rejection"); }
    catch (error) { expect(error).toMatchObject({ kind: "server", message }); }
  }
  expect(() => decodeSelectionCounts([{ priority: false, __count: 1 }], { ...field, required: true })).toThrow("Required selection tab field returned an unset group.");
});

/// (partial: real selection domains, URL tokens, keyboard and selected-count state)
test("selection tabs preserve base domains, use declared values and retain focus when counts refresh", () => {
  const panel = document.createElement("div"); panel.id = "actual-results"; document.body.append(panel);
  const changed: (string | undefined)[] = [];
  const tabs = createListTabs({ field, panel, initial: undefined, label: "Priority", enableCounters: true, expand: true, change: token => { changed.push(token); } });
  document.body.append(tabs.element);
  tabs.publish(decodeSelectionCounts([{ priority: "normal", __count: 3 }], field));
  const high = tabs.element.querySelector<HTMLButtonElement>('[data-tab-token="value:high"]')!;
  high.click();
  expect(changed).toEqual(["value:high"]);
  const base = [["name", "ilike", "needle"]];
  expect(tabs.rowDomain(base)).toEqual([...base, ["priority", "=", "high"]]);
  expect(base).toEqual([["name", "ilike", "needle"]]);
  expect(high.getAttribute("aria-selected")).toBe("true");
  expect(high.getAttribute("aria-controls")).toBe(panel.id);
  expect(panel.getAttribute("role")).toBe("tabpanel");
  expect(panel.getAttribute("aria-labelledby")).toBe(high.id);
  high.focus(); high.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowRight", bubbles: true }));
  expect(changed.at(-1)).toBeUndefined();
  expect(tabs.rowDomain(base)).toEqual(base);
  tabs.publish(decodeSelectionCounts([{ priority: false, __count: 1 }], field));
  expect(document.activeElement?.getAttribute("data-tab-token")).toBe("all");
  expect(document.activeElement?.getAttribute("aria-selected")).toBe("true");
  expect(document.activeElement?.textContent).toBe("All1");
  document.activeElement?.dispatchEvent(new KeyboardEvent("keydown", { key: "Home", bubbles: true }));
  expect(changed.at(-1)).toBeUndefined();
  expect(tabs.rowDomain(base)).toEqual(base);
});

/// (partial: invalid route state fails closed)
test("unknown selection tokens and absent required unset states are rejected", () => {
  expect(() => createListTabs({ field, initial: "value:missing", label: "Priority", enableCounters: true, expand: true, change: () => {} })).toThrow("Invalid list tab selection.");
  expect(() => createListTabs({ field: { ...field, required: true }, initial: "unset", label: "Priority", enableCounters: true, expand: true, change: () => {} })).toThrow("Invalid list tab selection.");
});

/// (partial: standard expand and counter options, zero-image default reset)
test("unexpanded categories hide missing values and return a vanished selection to All; counters can be disabled", () => {
  const tabs = createListTabs({ field, initial: "value:high", label: "Priority", enableCounters: false, expand: false, change: () => {} });
  document.body.append(tabs.element);
  const counts = decodeSelectionCounts([{ priority: "normal", __count: 2 }], field);
  expect(tabs.reconcile(counts)).toBe(true);
  tabs.publish(counts);
  expect(tabs.element.querySelector<HTMLButtonElement>('[data-tab-token="value:high"]')?.hidden).toBe(true);
  expect(tabs.element.querySelector('[aria-selected="true"]')?.getAttribute("data-tab-token")).toBe("all");
  expect(tabs.element.querySelector<HTMLElement>(".list-tab-count")?.hidden).toBe(true);
  expect(tabs.element.querySelector('[data-tab-token="value:normal"]')?.getAttribute("aria-label")).toBe("Normal");
  expect(tabs.rowDomain([])).toEqual([]);
  tabs.invalidate();
  expect(tabs.element.querySelector('[data-tab-token="all"]')?.getAttribute("aria-label")).toBe("All");
});
