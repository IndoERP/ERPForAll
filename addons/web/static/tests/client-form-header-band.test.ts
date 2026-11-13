/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { afterEach, expect, test, vi } from "vitest";
import { createFieldControl } from "../src/client/fields.js";
import type { Field, Node } from "../src/client/metadata.js";
import { renderFormLayout } from "../src/client/form-layout.js";
import { actionButton } from "../src/client/dom.js";
import { RpcClient } from "../src/client/transport.js";

const css = readFileSync(resolve(process.cwd(), "..", "addons/web/static/src/client/client.css"), "utf8");
const rpc = new RpcClient(async () => { throw new Error("Unexpected RPC"); }, () => {});
const field = (name: string) => ({ kind: "field", force_save: false, field: { name }, label: name, widget: "char" });
const renderField = (node: Extract<Node, { kind: "field" }>, parent: HTMLElement): void => { const input = document.createElement("input"); input.name = node.field.name; parent.append(input); };
const renderButton = (node: { label: string }, parent: HTMLElement): HTMLButtonElement => { const control = document.createElement("button"); control.type = "button"; control.textContent = node.label; parent.append(control); return control; };
afterEach(() => document.body.replaceChildren());

/// (partial: the smart-button box lands in the control row's host and every button carries an icon)
test("smart buttons render into the control row's host, each with an icon", () => {
  const stat = { kind: "stat_button", name: "open_lines", label: "Lines", variant: "secondary", confirm: null, invisible: null, children: [] };
  const nodes = [{ kind: "root", children: [{ kind: "sheet", children: [{ kind: "button_box", children: [stat] }] }] }] as unknown as Node[];
  const form = document.createElement("form"), smartHost = document.createElement("div");
  renderFormLayout(form, nodes, renderField, new Map(), renderButton as never, undefined, { smartHost });
  expect(form.querySelector(".form-button-box")).toBeNull();
  const button = smartHost.querySelector<HTMLElement>(":scope > .form-button-box > .form-stat-button")!;
  expect(button.querySelector(":scope > svg.erp-icon")?.getAttribute("data-icon")).toBe("layers");
});

/// (partial: the rail is a collapsed disclosure without a chatter, too)
test("the Record information rail is collapsed by default when the model has no thread", () => {
  const nodes = [{ kind: "root", children: [{ kind: "aside", children: [{ kind: "group", label: "Record information", columns: 2, colspan: 2, children: ["a", "b", "c", "d"].map(field) }] }] }] as unknown as Node[];
  const asideHost = document.createElement("aside");
  const labelled = (node: Extract<Node, { kind: "field" }>, parent: HTMLElement): void => { const label = document.createElement("label"); label.className = "field-label"; renderField(node, label); parent.append(label); };
  renderFormLayout(document.createElement("form"), nodes, labelled, new Map(), undefined, undefined, { asideHost });
  const aside = asideHost.querySelector<HTMLElement>(".form-aside")!;
  expect(aside.classList.contains("is-collapsed")).toBe(true);
  expect(aside.querySelector(".form-aside-toggle")?.textContent).toBe("Record information");
});

const radio = (options: Record<string, unknown>): Field & { widget: string; options: Record<string, unknown> } => ({ name: "format", label: "Format", kind: "selection", readonly: false, required: false, relation: null, relationField: null, selection: [["a", "A"], ["b", "B"]], widget: "radio", options });
const group = (options: Record<string, unknown>): HTMLElement => createFieldControl({ field: radio(options), value: "a", rpc, context: {}, change: vi.fn(), run: job => { void job(); } }).node.querySelector<HTMLElement>('[role="radiogroup"]')!;

/// (partial: a view that says horizontal:false keeps a column, otherwise the form lays the row out)
test("only an explicit horizontal:false marks a radio group as a column", () => {
  expect(group({}).dataset["explicitVertical"]).toBeUndefined();
  expect(group({ horizontal: false }).dataset["explicitVertical"]).toBe("");
});

const integer = (options: Record<string, unknown>): HTMLInputElement => createFieldControl({ field: { name: "year", label: "Year", kind: "integer", readonly: false, required: false, relation: null, relationField: null, selection: null, widget: "integer", options } as Field, value: 1974, rpc, context: {}, numericLocale: { decimal_point: ".", thousands_sep: ",", grouping: [3, 0] }, change: vi.fn(), run: job => { void job(); } }).node.querySelector("input")!;

/// (partial: a year is declared with the number input type; no server metadata)
test("an integer declared with type number shows no thousands separator, a plain one does", () => {
  expect(integer({ type: "number" }).value).toBe("1974");
  expect(integer({}).value).toBe("1,974");
});

/// (partial: density and affordance rules are tokens, not per-widget numbers)
test("the form stylesheet pins the 32px pitch, our radio, and hover-only edit affordances", () => {
  expect(css).toMatch(/\.record-form \{ --form-row-height: 28px; \}/);
  expect(css).toMatch(/\.record-form:not\(\.settings-form\) \.form-group-grid \{ row-gap: 4px; \}/);
  expect(css).toMatch(/\.field-radio-item input\[type="radio"\] \{[^}]*appearance: none/);
  expect(css).toMatch(/\.field-copy-clipboard:not\(:hover\):not\(:focus-within\) > button[^{]*\{ visibility: hidden; \}/);
  expect(css).toMatch(/--label: #3d4d59;/);
});

/// (partial: tabs that do not fit leave the row for a More menu and the selected tab stays)
test("a notebook row that overflows moves its last tabs into a More menu and never wraps", () => {
  const pages = ["a", "b", "c", "d", "e"].map(name => ({ kind: "page", name, label: name.toUpperCase(), children: [field(name)] }));
  const nodes = [{ kind: "root", children: [{ kind: "sheet", children: [{ kind: "notebook", children: pages }] }] }] as unknown as Node[];
  // No layout in jsdom: a row is 100px per shown tab and 250px wide.
  const widthOf = vi.spyOn(HTMLElement.prototype, "scrollWidth", "get").mockImplementation(function (this: HTMLElement) { return this.classList.contains("form-tabs") ? [...this.querySelectorAll<HTMLElement>(".form-tab")].filter(tab => !tab.hidden).length * 100 : 0; });
  const clientOf = vi.spyOn(HTMLElement.prototype, "clientWidth", "get").mockImplementation(function (this: HTMLElement) { return this.classList.contains("form-tabs") ? 250 : 0; });
  try {
    const form = document.createElement("form"); document.body.append(form);
    renderFormLayout(form, nodes, renderField, new Map());
    window.dispatchEvent(new Event("resize"));
    const shown = [...form.querySelectorAll<HTMLElement>(".form-tab")].filter(tab => !tab.hidden).map(tab => tab.textContent);
    expect(shown).toEqual(["A", "B"]);
    expect(form.querySelector<HTMLElement>(".form-tab-more")!.hidden).toBe(false);
    expect([...form.querySelectorAll<HTMLElement>(".form-tab-overflow-item")].filter(item => !item.hidden).map(item => item.textContent)).toEqual(["C", "D", "E"]);
    // Choosing a tab from the menu selects it, and it takes a place in the row.
    form.querySelectorAll<HTMLElement>(".form-tab-overflow-item")[3]!.click();
    expect(form.querySelector("[role=tab][aria-selected=true]")!.textContent).toBe("D");
    expect([...form.querySelectorAll<HTMLElement>(".form-tab")].filter(tab => !tab.hidden).map(tab => tab.textContent)).toContain("D");
  } finally { widthOf.mockRestore(); clientOf.mockRestore(); }
});

/// (partial: the tablist owns tabs only; More sits beside it and keeps focus through a fit)
test("the tab overflow lives beside the tablist and keeps focus when the row is fitted again", () => {
  const pages = ["a", "b", "c", "d"].map(name => ({ kind: "page", name, label: name.toUpperCase(), children: [field(name)] }));
  const nodes = [{ kind: "root", children: [{ kind: "sheet", children: [{ kind: "notebook", children: pages }] }] }] as unknown as Node[];
  const widthOf = vi.spyOn(HTMLElement.prototype, "scrollWidth", "get").mockImplementation(function (this: HTMLElement) { return this.classList.contains("form-tabs") ? [...this.querySelectorAll<HTMLElement>(".form-tab")].filter(tab => !tab.hidden).length * 100 : 0; });
  const clientOf = vi.spyOn(HTMLElement.prototype, "clientWidth", "get").mockImplementation(function (this: HTMLElement) { return this.classList.contains("form-tabs") ? 250 : 0; });
  try {
    const form = document.createElement("form"); document.body.append(form);
    renderFormLayout(form, nodes, renderField, new Map());
    const tablist = form.querySelector<HTMLElement>('[role="tablist"]')!;
    expect([...tablist.children].every(child => child.getAttribute("role") === "tab")).toBe(true);
    const more = form.querySelector<HTMLButtonElement>(".form-tab-more")!;
    expect(tablist.contains(more)).toBe(false);
    more.focus();
    window.dispatchEvent(new Event("resize"));
    expect(document.activeElement).toBe(more);
    expect(form.querySelectorAll(".form-tab-more")).toHaveLength(1);
    expect(form.querySelectorAll(".form-tab-overflow-menu")).toHaveLength(1);
  } finally { widthOf.mockRestore(); clientOf.mockRestore(); document.body.replaceChildren(); }
});

/// (partial: Duplicate is a built-in action with its own icon)
test("the Duplicate command carries the files icon like every other menu command", () => {
  const duplicate = actionButton("Duplicate", () => {}, "dropdown-item");
  expect(duplicate.textContent).toBe("Duplicate");
  expect(duplicate.querySelector(":scope > svg.erp-icon")?.getAttribute("data-icon")).toBe("files");
});

/// (partial: the stylesheet pins the quiet statusbar, the record menu and the shared chip rule)
test("the stylesheet pins plain statusbar stages, the dots record menu and one chip rule", () => {
  expect(css).toMatch(/\.statusbar-step\.is-passed \{ background: transparent; color: var\(--muted\); \}/);
  expect(css).toMatch(/\.record-menu-dots::before \{ top: -5px; \}/);
  expect(css).toMatch(/\.many2many-tags \.relation-chip \{ box-sizing: border-box; height: 22px;/);
  expect(css).toMatch(/\.form-tabs > \.form-tab\[hidden\] \{ display: none; \}/);
});
