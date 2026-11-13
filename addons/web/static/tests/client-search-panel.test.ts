/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test } from "vitest";
import { readdirSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { SearchModel } from "../src/client/search/search-model.js";
import { createSearchBar } from "../src/client/search/search-bar.js";
import type { Field } from "../src/client/metadata.js";

const field = (name: string, label: string, kind: Field["kind"] = "char"): Field =>
  ({ name, kind, label, required: false, readonly: false, relation: null, relationField: null, selection: null });
const model = (viewNames: string[] = ["name", "city"]): SearchModel => {
  const fields = new Map([["name", field("name", "Complete Name")], ["city", field("city", "City")], ["street2", field("street2", "Street2")], ["type", field("type", "Type")]]);
  return new SearchModel({ model: "t.partner", fields, viewFields: viewNames.map(n => fields.get(n)!) });
};
afterEach(() => document.body.replaceChildren());

const root = resolve(process.cwd(), "..", "addons/web/static/src/client");
const sources = (dir: string): string[] => readdirSync(dir, { withFileTypes: true }).flatMap(entry =>
  entry.name === "vendor" ? [] : entry.isDirectory() ? sources(join(dir, entry.name)) : entry.name.endsWith(".ts") ? [join(dir, entry.name)] : []);

test("no client source text shown to users names an internal task id or roadmap milestone", () => {
  const offenders: string[] = [];
  for (const file of sources(root)) {
    readFileSync(file, "utf8").split("\n").forEach((line, index) => {
      const code = line.trim();
      if (code.startsWith("//") || code.startsWith("*") || code.startsWith("/*")) return;
      if (/\bM[0-9]\.[A-Z][A-Z0-9]*(?:[.-][A-Z0-9]+)*\b|not available until|\buntil M[0-9]/.test(code.replace(/\s\/\/.*$/, ""))) offenders.push(`${file}:${index + 1}`);
    });
  }
  expect(offenders).toEqual([]);
});

test("the filter panel renders only sections that do something", () => {
  const bar = createSearchBar(model(), () => {});
  document.body.append(bar.element);
  bar.element.querySelector<HTMLButtonElement>(".search-menu-toggle")!.click();
  const panel = bar.element.querySelector<HTMLElement>(".erp-search-menu-panel")!;
  expect(panel.querySelectorAll(".erp-search-section")).toHaveLength(1);
  expect(panel.textContent).not.toMatch(/Group By|Favorites|not available/);
  bar.dispose();
});

test("a panel with nothing to offer has no toggle", () => {
  const bar = createSearchBar(new SearchModel({ model: "t.empty", fields: new Map() }), () => {});
  expect(bar.element.querySelector<HTMLButtonElement>(".search-menu-toggle")!.hidden).toBe(true);
  bar.dispose();
});

test("a field facet reads its label once and matches the label in the field selector", () => {
  const m = model(), bar = createSearchBar(m, () => {});
  document.body.append(bar.element);
  bar.input.value = "zzzz";
  bar.form.dispatchEvent(new Event("submit", { cancelable: true }));
  const chip = bar.element.querySelector(".search-facet-chip")!;
  expect(chip.textContent).toContain('Complete Name: "zzzz"');
  expect(chip.textContent!.match(/Complete Name/g)).toHaveLength(1);
  expect(bar.fieldSelect.querySelector(".search-field-trigger")!.textContent).toBe("Complete Name");
  expect(bar.fieldSelect.querySelector(".search-field-trigger")!.getAttribute("aria-label")).toBe("Search field: Complete Name");
  bar.dispose();
});

test("the field selector and the suggestions offer the view's searchable fields in the view's order", () => {
  const m = model(["city", "name"]), bar = createSearchBar(m, () => {});
  document.body.append(bar.element);
  expect([...bar.fieldSelect.querySelectorAll("[role=menuitemradio]")].map(o => o.textContent)).toEqual(["City", "Complete Name"]);
  expect(m.getAutocompleteItems("x").map(i => i.fieldName)).toEqual(["city", "name"]);
  bar.dispose();
});

test("the field selector is a menu dropdown, not a native select, and is keyboard operable", () => {
  const bar = createSearchBar(model(), () => {});
  document.body.append(bar.element);
  expect(bar.element.querySelector("select")).toBeNull();
  const trigger = bar.fieldSelect.querySelector<HTMLButtonElement>(".search-field-trigger")!;
  expect(trigger.getAttribute("aria-haspopup")).toBe("true");
  trigger.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));
  expect(trigger.getAttribute("aria-expanded")).toBe("true");
  bar.fieldSelect.querySelector<HTMLButtonElement>('[data-field="city"]')!.click();
  expect(bar.fieldSelect.value).toBe("city");
  expect(trigger.textContent).toBe("City");
  expect(trigger.getAttribute("aria-expanded")).toBe("false");
  bar.dispose();
});

test("the facet line never wraps and the selection summary is not a box", () => {
  const css = readFileSync(join(root, "client.css"), "utf8");
  expect(css).toMatch(/\.search-chips \{[^}]*flex-wrap: nowrap/);
  expect(css).toMatch(/\.erp-search-input-container \{[^}]*flex-wrap: nowrap/);
  expect(css).not.toMatch(/\.selection-box \{[^}]*(border|background)/);
});
