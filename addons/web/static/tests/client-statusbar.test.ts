/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { expect, test, vi } from "vitest";
import "../src/client/fields.js";
import { describeViews, type Field } from "../src/client/metadata.js";
import { statusbarControl } from "../src/client/statusbar.js";
import { ClientError, RpcClient } from "../src/client/transport.js";
import { renderFormLayout } from "../src/client/form-layout.js";
import { RecordForm } from "../src/client/record-form.js";
import { RecordState } from "../src/client/record.js";
const refuses = (job: () => unknown, kind: ClientError["kind"], message: string): void => {
  let caught: unknown;
  try { job(); } catch (error) { caught = error; }
  expect(caught).toBeInstanceOf(ClientError);
  expect(caught).toMatchObject({ kind, message });
};
const field: Field = { name: "phase", kind: "selection", label: "Phase", readonly: false, required: false, relation: null, relationField: null, selection: [["draft", "Draft"], ["review", "Review"], ["done", "Done"]] };
const render = (value: unknown, overrides: Partial<Field> = {}) => {
  const change = vi.fn();
  const handle = statusbarControl({ field: { ...field, ...overrides }, value, change, rpc: new RpcClient(fetch, () => {}), context: {}, run: job => { void job(); }, report: () => {} });
  return { node: handle.node, change };
};
/// (partial: model order/labels and current-state retention render without hardcoded state identity)
test("statusbar filters choices in model order and always retains the actual current state", () => {
  const { node } = render("review", { statusbarVisible: ["done", "draft"] });
  expect([...node.querySelectorAll(".statusbar-step")].map(step => step.textContent)).toEqual(["Draft", "Review", "Done"]);
  expect(node.querySelector('[aria-current="step"]')?.textContent).toBe("Review");
  expect(node.getAttribute("aria-label")).toBe("Phase");
  expect(render(false, { statusbarVisible: ["done", "draft"] }).node.querySelectorAll(".statusbar-step")).toHaveLength(2);
  expect(render(false).node.querySelector("[aria-current]")).toBeNull();
});
/// (partial: display-only default never edits a record even when model field is writable)
test("statusbar choices are inert without explicit clickability", () => {
  for (const policy of [false, true]) {
    const status = render("draft", { readonly: policy });
    expect(status.node.querySelectorAll("button")).toHaveLength(0);
    for (const step of status.node.querySelectorAll<HTMLElement>(".statusbar-step")) { step.click(); step.dispatchEvent(new Event("click")); }
    expect(status.change).not.toHaveBeenCalled();
    expect(status.node.dataset["clickable"]).toBe("false");
  }
  // A read-only field stays inert even when the occurrence asks for clicks.
  const locked = render("draft", { statusbarClickable: true, readonly: true });
  expect(locked.node.querySelectorAll("button")).toHaveLength(0);
  refuses(() => render("missing"), "server", "Unknown statusbar value for phase.");
});
const clickable = (value: unknown, overrides: Partial<Field> = {}) => render(value, { statusbarClickable: true, ...overrides });
const step = (node: HTMLElement, key: string) => node.querySelector<HTMLElement>(`.statusbar-track [data-value="${key}"]`)!;
test("clickable statusbar: passed, current and upcoming steps; only other steps are buttons and a click changes the value", () => {
  const { node, change } = clickable("review");
  expect(step(node, "draft").className).toContain("is-passed");
  expect(step(node, "review").className).toContain("is-current");
  expect(step(node, "done").className).not.toMatch(/is-(passed|current)/);
  expect(step(node, "review").tagName).toBe("SPAN");
  expect(step(node, "draft").tagName).toBe("BUTTON");
  step(node, "review").click();
  expect(change).not.toHaveBeenCalled();
  step(node, "done").click();
  expect(change).toHaveBeenCalledExactlyOnceWith("done");
});
test("clickable statusbar has one tab stop; arrows, Home and End move focus only and Enter activates", () => {
  const { node, change } = clickable("review");
  document.body.append(node);
  const draft = step(node, "draft") as HTMLButtonElement, done = step(node, "done") as HTMLButtonElement;
  expect([draft.tabIndex, done.tabIndex]).toEqual([0, -1]);
  draft.focus();
  draft.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowRight", bubbles: true, cancelable: true }));
  expect(document.activeElement).toBe(done);
  expect([draft.tabIndex, done.tabIndex]).toEqual([-1, 0]);
  done.dispatchEvent(new KeyboardEvent("keydown", { key: "Home", bubbles: true, cancelable: true }));
  expect(document.activeElement).toBe(draft);
  draft.dispatchEvent(new KeyboardEvent("keydown", { key: "End", bubbles: true, cancelable: true }));
  expect(document.activeElement).toBe(done);
  done.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowRight", bubbles: true, cancelable: true }));
  expect(document.activeElement).toBe(done);
  expect(change).not.toHaveBeenCalled();
  node.remove();
});
test("folded states sit behind a More states control at the track start; the narrow control shows the current state", () => {
  const { node, change } = clickable("review", { statusbarFolded: ["done"] });
  const first = node.querySelector(".statusbar-track > .statusbar-item")!;
  expect(first.querySelector(".statusbar-more")?.getAttribute("aria-label")).toBe("More states");
  expect(node.querySelector('.statusbar-track > .statusbar-item > [data-value="done"]')).toBeNull();
  expect([...first.querySelectorAll<HTMLElement>(".statusbar-menu-item")].map(item => item.dataset["state"])).toEqual(["done"]);
  first.querySelector<HTMLElement>('.statusbar-menu-item[data-state="done"]')!.click();
  expect(change).toHaveBeenCalledExactlyOnceWith("done");
  // The narrow control: the current state, opening the full list (current item disabled); display-only it is plain text.
  const trigger = node.querySelector<HTMLButtonElement>(".statusbar-compact-trigger")!;
  expect(trigger.textContent).toBe("Review");
  expect([...node.querySelectorAll<HTMLButtonElement>(".statusbar-compact .statusbar-menu-item")].map(item => [item.dataset["state"], item.disabled])).toEqual([["draft", false], ["review", true], ["done", false]]);
  expect(render("review").node.querySelector(".statusbar-compact-value")?.textContent).toBe("Review");
  expect(render("review").node.querySelector(".statusbar-compact button")).toBeNull();
  const css = readFileSync(resolve(process.cwd(), "../addons/web/static/src/client/client.css"), "utf8");
  expect(css).toMatch(/@media \(max-width: 700px\) \{ \.statusbar-track \{ display: none; \} \.statusbar-compact \{ display: flex; \} \}/);
});
test("the current state stays visible even when it is folded", () => {
  const { node } = clickable("done", { statusbarFolded: ["done"] });
  expect(node.querySelector('.statusbar-track [data-value="done"]')?.getAttribute("aria-current")).toBe("step");
  expect(node.querySelector(".statusbar-more")).toBeNull();
});
const description = () => ({ wire_major: 1, registry_version: "r", description_version: "d", model: "test.flow", fields: { "test.flow": { phase: { type: "selection", label: "Phase", readonly: false, required: false, selection: field.selection, relation: null, relation_field: null } } }, views: [{ identity: { kind: "stored", id: 1 }, type: "form", renderer: "web.form", schema_major: 1, root: 0, nodes: [{ kind: "form", record_mode: "record", children: [1] }, { kind: "header", children: [2] }, { kind: "field", field: { model: "test.flow", name: "phase" }, widget: "statusbar", options: { visible: ["draft", "done"] }, label: "Phase", force_save: false, modifiers: { readonly: null, required: null, invisible: null } }] }], notices: [] });
/// (partial: strict consumer and shared form layout expose an authorized selection header)
test("real metadata drives a header field even without header action buttons", () => {
  const view = describeViews(description(), "test.flow", "r").views.get("form")!;
  expect(view.fields[0]?.statusbarVisible).toEqual(["draft", "done"]);
  const form = document.createElement("form");
  renderFormLayout(form, view.nodes, (node, parent) => parent.append(render("draft", node.field).node), new Map());
  expect(form.querySelectorAll(".form-header .form-statusbar")).toHaveLength(1);
  expect(form.querySelector(".form-header-actions")).toBeNull();
});
/// (partial: client refuses unsafe options and misplaced statusbars)
test("consumer rejects unknown, duplicate, empty and malformed visible keys", () => {
  for (const visible of [["missing"], ["draft", "draft"], [], "draft"]) {
    const d = description(); Object.assign(d.views[0]!.nodes[2]!.options!, { visible });
    refuses(() => describeViews(d, "test.flow", "r"), typeof visible === "string" ? "server" : "unsupported", "Invalid statusbar visible keys.");
  }
  const unknown = description(); Object.assign(unknown.views[0]!.nodes[2]!.options!, { draggable: true });
  refuses(() => describeViews(unknown, "test.flow", "r"), "unsupported", "Unknown statusbar options member: draggable.");
  for (const folded of [["missing"], ["draft", "draft"]]) {
    const d = description(); Object.assign(d.views[0]!.nodes[2]!.options!, { folded });
    refuses(() => describeViews(d, "test.flow", "r"), "unsupported", "Invalid statusbar folded keys.");
  }
  const admitted = description(); Object.assign(admitted.views[0]!.nodes[2]!.options!, { folded: ["done"], clickable: true });
  expect(describeViews(admitted, "test.flow", "r").views.get("form")!.fields[0]).toMatchObject({ statusbarFolded: ["done"], statusbarClickable: true });
  const missing = description(); Object.assign(missing.views[0]!.nodes[2]!, { options: {} });
  refuses(() => describeViews(missing, "test.flow", "r"), "server", "Missing statusbar options member: visible.");
  const misplaced = description(); Object.assign(misplaced.views[0]!.nodes[1]!, { kind: "sheet" });
  refuses(() => describeViews(misplaced, "test.flow", "r"), "unsupported", "The statusbar widget requires a selection field in a record form header.");
});

/// (partial: a selection named name never becomes the editable title)
test("header selection named name remains a statusbar while the character title lifts", () => {
  const selection = { ...field, name: "name" }, caption: Field = { ...field, name: "caption", kind: "char", label: "Caption", selection: null };
  const form = document.createElement("form"), headerHost = document.createElement("div"), titleHost = document.createElement("div"), notebookTabsHost = document.createElement("div");
  const rpc = new RpcClient(fetch, () => {}), state = new RecordState(rpc, "fixture.flow", [selection, caption], {});
  state.values = { name: "draft", caption: "Actual title" };
  const surface = new RecordForm({ form, headerHost, titleHost, notebookTabsHost, state, rpc, context: {},
    view: { type: "form", mode: "record", fields: [selection, caption], nodes: [{ kind: "root", children: [
      { kind: "header", children: [{ kind: "field", field: selection, label: selection.label, widget: "statusbar" }] },
      { kind: "sheet", children: [{ kind: "field", field: caption, label: caption.label, widget: "char" },
        { kind: "notebook", children: [{ kind: "page", name: "details", label: "Details", children: [] }] }
      ] }
    ] }] }, run: job => { void job(); }, report: error => { throw error; }, openRecord: async () => {}, change: () => {}, button: () => {} });
  surface.render();
  expect(titleHost.querySelector<HTMLInputElement>('input[name="caption"]')?.value).toBe("Actual title");
  expect(headerHost.querySelector('.form-header > .form-statusbar')).not.toBeNull();
  expect(headerHost.querySelector('.form-title-field')).toBeNull();
  expect(surface.controls()).toContain(notebookTabsHost.querySelector('[role="tab"]'));
  surface.dispose();
});
