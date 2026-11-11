/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import axe from "axe-core";
// The app registers the scalar field widgets that one2many cells use (fields.ts) before any form renders.
import "../../addons/web/static/src/client/fields.js";
import "../../addons/web/static/src/client/x2many.js";
import { fieldWidgets } from "../../addons/web/static/src/client/registry.js";
import type { Field, View } from "../../addons/web/static/src/client/metadata.js";
import { addonsView } from "../../addons/web/static/src/client/services.js";
import { RpcClient } from "../../addons/web/static/src/client/transport.js";

const cleanups: (() => void)[] = [];
afterEach(() => {
  cleanups.splice(0).reverse().forEach(dispose => dispose());
  document.body.replaceChildren();
});

function page(title: string): HTMLElement {
  const main = document.createElement("main"), heading = document.createElement("h1");
  heading.textContent = title; main.append(heading); document.body.append(main);
  return main;
}

function rpc(value: unknown): RpcClient {
  return new RpcClient(async () => new Response(JSON.stringify(value), {
    headers: { "Content-Type": "application/json" }
  }), () => {});
}

const title: Field = {
  name: "title", kind: "char", label: "Title", required: false, readonly: false,
  invisible: false, relation: null, relationField: null, selection: null
};
const lineForm: View = {
  type: "form", mode: "record", fields: [title],
  nodes: [{ kind: "root", children: [{ kind: "field", field: title, label: "Title", widget: "char" }] }]
};

function lines(value: unknown[], readonly: boolean): HTMLElement {
  const field: Field = {
    name: "line_ids", kind: "one2many", label: "Lines", required: false, readonly,
    invisible: false, relation: "test.line", relationField: "parent_id", selection: null,
    subview: { model: "test.line", list: [title], form: lineForm, fields: new Map([[title.name, title]]) }
  };
  const handle = fieldWidgets.get("one2many").render({
    field, value, rpc: rpc({}), context: {}, change: vi.fn(), run: vi.fn(), report: vi.fn()
  });
  if (handle.dispose) cleanups.push(handle.dispose);
  page("Parent record").append(handle.node);
  return handle.node;
}

test.each([
  { label: "empty", items: [] },
  { label: "populated", items: [{ id: 1, title: "First line" }] }
])("editable one2many action headers have text and column scope ($label)", async ({ items }) => {
  const node = lines(items, false);
  const header = node.querySelector<HTMLTableCellElement>("th.x2many-actions")!;
  expect(header.textContent?.trim()).toBe("Actions");
  expect(header.scope).toBe("col");
  expect(node.querySelectorAll("thead th")).toHaveLength(2);
  const report = await axe.run(node, { runOnly: { type: "rule", values: ["empty-table-header"] } });
  expect(report.violations.map(violation => violation.id)).toEqual([]);
});

test("empty editable and readonly one2many tables retain their column counts", () => {
  const editable = lines([], false), readonly = lines([], true);
  expect(editable.querySelector<HTMLTableCellElement>(".x2many-empty td")?.colSpan).toBe(2);
  expect(readonly.querySelector(".x2many-actions")).toBeNull();
  expect(readonly.querySelectorAll("thead th")).toHaveLength(1);
  expect(readonly.querySelector<HTMLTableCellElement>(".x2many-empty td")?.colSpan).toBe(1);
});

test("readonly populated one2many tables have no action column", () => {
  const node = lines([{ id: 1, title: "First line" }], true);
  expect(node.querySelector(".x2many-actions")).toBeNull();
  expect(node.querySelectorAll("thead th")).toHaveLength(1);
  expect(node.querySelectorAll("tbody tr:first-child td")).toHaveLength(1);
  expect(node.querySelector("tbody td")?.textContent).toBe("First line");
});

test("Addons headings follow the page in ungrouped and grouped results", async () => {
  const main = page("Addons"), host = document.createElement("div"); main.append(host);
  await addonsView(host, rpc({ wire_major: 1, addons: [{
    name: "notes", label: "Notes", summary: "Notes", category: "Productivity", author: "ERPForAll",
    application: true, website: "", license: "LGPL-3", depends: [], version: "1", state: "installed", installable: true
  }] }), vi.fn(), vi.fn(), {});
  expect([...host.querySelectorAll(".addons-panel-title")].map(heading => [heading.tagName, heading.textContent]))
    .toEqual([["H2", "Apps"], ["H2", "Categories"], ["H2", "Demo data"]]);
  expect(host.querySelector(".addon-name")?.tagName).toBe("H2");
  let report = await axe.run(main, { runOnly: { type: "rule", values: ["heading-order"] } });
  expect(report.violations.map(violation => violation.id)).toEqual([]);

  const group = host.querySelector<HTMLSelectElement>(".addons-groupby")!;
  group.value = "category"; group.dispatchEvent(new Event("change"));
  expect(host.querySelector(".addons-group-title")?.tagName).toBe("H2");
  expect(host.querySelector(".addon-name")?.tagName).toBe("H3");
  report = await axe.run(main, { runOnly: { type: "rule", values: ["heading-order"] } });
  expect(report.violations.map(violation => violation.id)).toEqual([]);
});
