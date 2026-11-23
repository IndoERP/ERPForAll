/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test } from "vitest";
import { describeViews, type Node, type PageNode } from "../src/client/metadata.js";
import { effectiveNodes } from "../src/client/field-modifiers.js";
import { renderFormLayout } from "../src/client/form-layout.js";

const lockedExpression = { schema: 1, nodes: [{ kind: "name", name: "locked" }] };
const fieldMeta = (type: string, label: string) => ({ type, label, required: false, readonly: false, relation: null, relation_field: null, selection: null });
const fieldNode = (name: string, widget: string, label: string) => ({ kind: "field", force_save: false, field: { model: "test.note", name }, widget, options: {}, modifiers: { readonly: null, required: null, invisible: null }, label });

/** A form with a boolean `locked` and a notebook whose second page hides while the record is locked. */
function description(page: Record<string, unknown> = { invisible: lockedExpression }): unknown {
  return {
    wire_major: 1, registry_version: "r1", description_version: "d1", model: "test.note", notices: [],
    fields: { "test.note": { locked: fieldMeta("boolean", "Locked"), name: fieldMeta("char", "Name"), note: fieldMeta("char", "Note") } },
    views: [{
      identity: { kind: "stored", id: 1 }, type: "form", schema_major: 1, renderer: "web.form", root: 0,
      nodes: [
        { kind: "form", record_mode: "record", children: [1, 2] },
        fieldNode("locked", "boolean", "Locked"),
        { kind: "notebook", children: [3, 4] },
        { kind: "page", name: "main", label: "Main", children: [5] },
        { kind: "page", name: "more", label: "More", children: [6], ...page },
        fieldNode("name", "char", "Name"),
        fieldNode("note", "char", "Note"),
      ]
    }]
  };
}

function formView(raw: unknown = description()) {
  const view = describeViews(raw, "test.note", "r1").views.get("form")!;
  return view;
}

function pages(nodes: readonly Node[]): PageNode[] {
  return nodes.flatMap(node => node.kind === "page" ? [node] : "children" in node ? pages(node.children) : []);
}

const renderField = (node: Extract<Node, { kind: "field" }>, parent: HTMLElement): void => {
  const input = document.createElement("input"); input.name = node.field.name; parent.append(input);
};

afterEach(() => document.body.replaceChildren());

test("a page's invisible expression is decoded; a page without one has none", () => {
  const [main, more] = pages(formView().nodes);
  expect(main!.invisible).toBeNull();
  expect(more!.invisible).not.toBeNull();
});

test("an unknown page member is still refused", () => {
  expect(() => formView(description({ hidden: true }))).toThrow();
});

test("the page hides for a record whose expression is true and shows for one whose expression is false", () => {
  const view = formView();
  const hidden = pages(effectiveNodes(view.nodes, view.fields, { locked: true, name: "A", note: "" }));
  expect(hidden.map(page => page.hidden)).toEqual([false, true]);
  const shown = pages(effectiveNodes(view.nodes, view.fields, { locked: false, name: "A", note: "" }));
  expect(shown.map(page => page.hidden)).toEqual([false, false]);
});

test("a hidden page has no tab and no panel, and the selection moves to a shown page", () => {
  const view = formView();
  const selected = new Map<Node, number>();
  const notebook = (nodes: readonly Node[]): Node => nodes.flatMap(node => node.kind === "notebook" ? [node] : "children" in node ? [notebook(node.children)] : [])[0]!;
  // The user was on More; the source notebook node keys the selection.
  selected.set(notebook(view.nodes), 1);
  const host = document.createElement("form"); document.body.append(host);
  // Selection is keyed by node identity: map the effective notebook back to the declared one like the record form does.
  const keyed = new Map<Node, number>();
  const render = (values: Record<string, unknown>) => {
    host.replaceChildren();
    const effective = effectiveNodes(view.nodes, view.fields, values);
    keyed.clear(); keyed.set(notebook(effective), selected.get(notebook(view.nodes))!);
    const layout = renderFormLayout(host, effective, renderField, keyed);
    selected.set(notebook(view.nodes), keyed.get(notebook(effective))!);
    return layout;
  };

  const shown = render({ locked: false, name: "A", note: "" });
  expect([...host.querySelectorAll(".form-tab")].map(tab => tab.textContent)).toEqual(["Main", "More"]);
  expect(host.querySelector(".form-tab[aria-selected='true']")!.textContent).toBe("More");
  expect(host.querySelector("input[name='note']")).not.toBeNull();
  shown.dispose();

  const hidden = render({ locked: true, name: "A", note: "" });
  expect([...host.querySelectorAll(".form-tab")].map(tab => tab.textContent)).toEqual(["Main"]);
  expect(host.querySelector(".form-tab[aria-selected='true']")!.textContent).toBe("Main");
  expect(host.querySelectorAll(".form-tab-panel")).toHaveLength(1);
  expect(host.querySelector<HTMLElement>(".form-tab-panel")!.hidden).toBe(false);
  expect(host.querySelector("input[name='note']")).toBeNull();
  expect(selected.get(notebook(view.nodes))).toBe(0);
  hidden.dispose();
});

test("keyboard navigation moves between the shown tabs only", () => {
  const raw = description() as { views: { nodes: Record<string, unknown>[] }[] };
  const nodes = raw.views[0]!.nodes;
  // A third, always shown page after the hidden one.
  nodes[2] = { kind: "notebook", children: [3, 4, 7] };
  nodes.push({ kind: "page", name: "last", label: "Last", children: [8] }, fieldNode("name", "char", "Name"));
  const view = formView(raw);
  const host = document.createElement("form"); document.body.append(host);
  const layout = renderFormLayout(host, effectiveNodes(view.nodes, view.fields, { locked: true, name: "A", note: "" }), renderField, new Map());
  expect([...host.querySelectorAll(".form-tab")].map(tab => tab.textContent)).toEqual(["Main", "Last"]);
  const main = host.querySelector<HTMLElement>(".form-tab")!;
  main.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowRight", bubbles: true, cancelable: true }));
  expect(host.querySelector(".form-tab[aria-selected='true']")!.textContent).toBe("Last");
  layout.dispose();
});

test("a notebook whose pages are all hidden is not shown", () => {
  const raw = description() as { views: { nodes: Record<string, unknown>[] }[] };
  raw.views[0]!.nodes[3] = { kind: "page", name: "main", label: "Main", children: [5], invisible: lockedExpression };
  const view = formView(raw);
  const host = document.createElement("form"); document.body.append(host);
  const layout = renderFormLayout(host, effectiveNodes(view.nodes, view.fields, { locked: true, name: "A", note: "" }), renderField, new Map());
  expect(host.querySelector(".form-notebook")).toBeNull();
  expect(host.querySelector(".form-tabs")).toBeNull();
  layout.dispose();
});
