/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import {
  createTreeEditor,
  type TreeEditorHandle,
} from "../src/client/tree-editor.js";
import type { ConditionNode } from "../src/client/condition-tree.js";
import type { Field } from "../src/client/metadata.js";
import { defaultTreeEditorProvider, type TreeValueEditorOptions } from "../src/client/tree-editor-fields.js";
import { MessageServices } from "../src/client/messages.js";
import { auditComponent } from "../../../../web/testing/axe.js";
let messages: MessageServices;
const handles: TreeEditorHandle[] = [];
const field = (name: string, kind: Field["kind"] = "char"): Field => ({
  name,
  kind,
  label: name === "name" ? "Reference" : "Amount",
  required: false,
  readonly: false,
  selection: null,
  relation: null,
  relationField: null,
});
const fields = new Map([["name", field("name")], [
  "amount",
  field("amount", "integer"),
]]);
const initial = (): ConditionNode => ({
  id: "root",
  kind: "and",
  children: [{
    id: "a",
    kind: "condition",
    path: "name",
    operator: "=",
    value: { kind: "string", value: "Original" },
  }],
});
const env = () => ({ messages, report: vi.fn() });
const control = <T extends HTMLElement>(
  node: HTMLElement,
  selector: string,
): T => {
  const value = node.querySelector<T>(selector);
  if (!value) throw new Error(`Missing fixture control ${selector}`);
  return value;
};
beforeEach(() => {
  messages = new MessageServices(document.body, vi.fn());
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", {
    configurable: true,
    value(this: HTMLDialogElement) {
      this.open = true;
    },
  });
  Object.defineProperty(HTMLDialogElement.prototype, "close", {
    configurable: true,
    value(this: HTMLDialogElement) {
      this.open = false;
    },
  });
});
afterEach(() => {
  handles.splice(0).forEach((handle) => handle.dispose());
  messages.dispose();
  document.body.replaceChildren();
});
async function fixture(
  change = vi.fn<(value: ConditionNode) => void | Promise<void>>(),
  readonly = false,
) {
  const environment = env();
  const editor = createTreeEditor({
    environment,
    label: "Rules",
    model: "sale.order",
    value: initial(),
    fields: () => fields,
    change,
    readonly,
  });
  handles.push(editor);
  document.body.append(editor.node);
  await editor.ready;
  return { editor, environment, change };
}
/// (partial: editable tree mutation and accessible controls; browser touch layout separate)
test("tree actions add, alternate nesting, change connector and prune deleted ancestors", async () => {
  const { editor, change } = await fixture();
  expect(
    editor.node.querySelector(".editor-model-field-selector .editor-label")
      ?.textContent,
  ).toBe("Field");
  control<HTMLButtonElement>(
    editor.node,
    '[data-tree-node="a"] [data-tree-action="nest"]',
  ).click();
  await vi.waitFor(() => expect(change).toHaveBeenCalledTimes(1));
  const nested = change.mock.calls[0]![0];
  expect(nested.kind).toBe("and");
  if (nested.kind !== "and") throw new Error("Expected root AND");
  expect(nested.children[0]).toMatchObject({
    kind: "or",
    children: [{ id: "a", path: "name" }],
  });
  await vi.waitFor(() =>
    expect(editor.node.querySelector('[data-tree-node="a"]')).not.toBeNull()
  );
  control<HTMLButtonElement>(
    editor.node,
    '[data-tree-node="root"] > .editor-tree-actions [data-tree-action="add"]',
  ).click();
  await vi.waitFor(() => expect(change).toHaveBeenCalledTimes(2));
  const connector = control<HTMLSelectElement>(
    editor.node,
    '[data-tree-node="root"] > .editor-tree-controls [data-tree-connector]',
  );
  connector.value = "or";
  connector.dispatchEvent(new Event("change"));
  await vi.waitFor(() => expect(change).toHaveBeenCalledTimes(3));
  expect(change.mock.calls[2]![0].kind).toBe("or");
  control<HTMLButtonElement>(
    editor.node,
    '[data-tree-node="a"] [data-tree-action="delete"]',
  ).click();
  await vi.waitFor(() => expect(change).toHaveBeenCalledTimes(4));
  expect(editor.node.querySelector('[data-tree-node="a"]')).toBeNull();
  expect(document.activeElement?.closest(".editor-tree")).toBe(editor.node);
  await expect(auditComponent(editor.node)).resolves.toBeDefined();
});
test("readonly descriptions contain safe text with no mutation controls", async () => {
  const { editor, change } = await fixture(undefined, true);
  expect(editor.node.textContent).toContain("Reference");
  expect(editor.node.textContent).toContain("Original");
  expect(editor.node.querySelector("input,select,textarea,button")).toBeNull();
  expect(change).not.toHaveBeenCalled();
  await expect(auditComponent(editor.node)).resolves.toBeDefined();
});
test("callback refusal retains prior value and reports visible failure without replay", async () => {
  const failure = new Error("Save refused");
  const change = vi.fn<(value: ConditionNode) => Promise<void>>()
    .mockRejectedValue(failure);
  const { editor, environment } = await fixture(change);
  const input = control<HTMLInputElement>(
    editor.node,
    '[data-tree-node="a"] input[data-tree-value]',
  );
  input.value = "Draft";
  input.dispatchEvent(new Event("change"));
  await vi.waitFor(() =>
    expect(environment.report).toHaveBeenCalledWith(failure)
  );
  expect(editor.node.querySelector('[role="alert"]')?.textContent).toContain(
    "Save refused",
  );
  expect(
    control<HTMLInputElement>(
      editor.node,
      '[data-tree-node="a"] input[data-tree-value]',
    ).value,
  ).toBe("Original");
  expect(change).toHaveBeenCalledTimes(1);
});
test("owner replacement supersedes pending user callback without emitting another edit", async () => {
  let release!: () => void;
  const change = vi.fn(() =>
    new Promise<void>((resolve) => {
      release = resolve;
    })
  );
  const { editor } = await fixture(change);
  const input = control<HTMLInputElement>(
    editor.node,
    '[data-tree-node="a"] input[data-tree-value]',
  );
  input.value = "Draft";
  input.dispatchEvent(new Event("change"));
  await vi.waitFor(() => expect(change).toHaveBeenCalledTimes(1));
  await editor.setValue({ id: "replacement", kind: "constant", value: false });
  release();
  await Promise.resolve();
  expect(editor.node.querySelector('[data-tree-node="replacement"]')).not
    .toBeNull();
  expect(editor.node.querySelector('[data-tree-node="a"]')).toBeNull();
  expect(change).toHaveBeenCalledTimes(1);
});
test("field choice atomically resets operator and literal using admitted metadata", async () => {
  const { editor, change } = await fixture();
  control<HTMLButtonElement>(
    editor.node,
    '[data-tree-node="a"] [data-field-open]',
  ).click();
  await vi.waitFor(() =>
    expect(document.querySelector('[data-field-name="amount"]')).not.toBeNull()
  );
  control<HTMLElement>(document.body, '[data-field-name="amount"]').click();
  await vi.waitFor(() => expect(change).toHaveBeenCalledTimes(1));
  expect(change.mock.calls[0]![0]).toMatchObject({
    children: [{
      id: "a",
      path: "amount",
      operator: "=",
      value: { kind: "integer", value: "0" },
    }],
  });
});
test("empty metadata refuses add visibly and disposed metadata cannot mutate DOM", async () => {
  let resolve!: (fields: ReadonlyMap<string, Field>) => void;
  const environment = env(), change = vi.fn();
  const editor = createTreeEditor({
    environment,
    label: "Rules",
    model: "sale.order",
    value: { id: "root", kind: "and", children: [] },
    fields: () =>
      new Promise<ReadonlyMap<string, Field>>((yes) => {
        resolve = yes;
      }),
    change,
  });
  handles.push(editor);
  document.body.append(editor.node);
  control<HTMLButtonElement>(editor.node, '[data-tree-action="add"]').click();
  await vi.waitFor(() => expect(resolve).toBeDefined());
  editor.dispose();
  const snapshot = editor.node.innerHTML;
  resolve(fields);
  await editor.ready;
  await Promise.resolve();
  expect(editor.node.innerHTML).toBe(snapshot);
  expect(change).not.toHaveBeenCalled();
  const unavailable = createTreeEditor({
    environment,
    label: "Empty",
    model: "sale.order",
    value: { id: "empty", kind: "and", children: [] },
    fields: () => new Map<string, Field>(),
    change,
  });
  handles.push(unavailable);
  document.body.append(unavailable.node);
  await unavailable.ready;
  control<HTMLButtonElement>(unavailable.node, '[data-tree-action="add"]')
    .click();
  await vi.waitFor(() =>
    expect(unavailable.node.querySelector('[role="alert"]')?.textContent)
      .toContain("No fields are available")
  );
  expect(change).not.toHaveBeenCalled();
});
test("operator changes replace incompatible scalar values and preserve compatible membership values", async () => {
  const { editor, change } = await fixture();
  const operator = control<HTMLSelectElement>(
    editor.node,
    '[data-tree-node="a"] [data-tree-operator]',
  );
  operator.value = "in";
  operator.dispatchEvent(new Event("change"));
  await vi.waitFor(() => expect(change).toHaveBeenCalledTimes(1));
  expect(change.mock.calls[0]![0]).toMatchObject({
    children: [{ operator: "in", value: { kind: "list", items: [] } }],
  });
  const next = control<HTMLSelectElement>(
    editor.node,
    '[data-tree-node="a"] [data-tree-operator]',
  );
  next.value = "not in";
  next.dispatchEvent(new Event("change"));
  await vi.waitFor(() => expect(change).toHaveBeenCalledTimes(2));
  expect(change.mock.calls[1]![0]).toMatchObject({
    children: [{ operator: "not in", value: { kind: "list", items: [] } }],
  });
});
test("obsolete metadata completion cannot replace a newer owner tree", async () => {
  let release!: (value: ReadonlyMap<string, Field>) => void;
  const metadata = new Promise<ReadonlyMap<string, Field>>((resolve) => {
    release = resolve;
  });
  const environment = env(), change = vi.fn();
  const editor = createTreeEditor({
    environment,
    label: "Rules",
    model: "sale.order",
    value: initial(),
    fields: () => metadata,
    change,
  });
  handles.push(editor);
  document.body.append(editor.node);
  await editor.setValue({ id: "new", kind: "constant", value: true });
  release(fields);
  await editor.ready;
  expect(editor.node.querySelector('[data-tree-node="new"]')).not.toBeNull();
  expect(editor.node.querySelector('[data-tree-node="a"]')).toBeNull();
  expect(change).not.toHaveBeenCalled();
});
test("pending callback admits one value edit and disposal suppresses completion", async () => {
  let release!: () => void;
  const change = vi.fn(() =>
    new Promise<void>((resolve) => {
      release = resolve;
    })
  );
  const { editor } = await fixture(change);
  const input = control<HTMLInputElement>(
    editor.node,
    '[data-tree-node="a"] input[data-tree-value]',
  );
  input.value = "first";
  input.dispatchEvent(new Event("change"));
  input.value = "second";
  input.dispatchEvent(new Event("change"));
  await vi.waitFor(() => expect(change).toHaveBeenCalledTimes(1));
  expect(editor.node.getAttribute("aria-busy")).toBe("true");
  editor.dispose();
  const before = editor.node.innerHTML;
  release();
  await Promise.resolve();
  await Promise.resolve();
  expect(editor.node.innerHTML).toBe(before);
  expect(change).toHaveBeenCalledTimes(1);
});
test("unknown tree options fail with precise validation errors", () => {
  const environment = env();
  expect(() =>
    createTreeEditor({
      ...{ unexpected: true },
      environment,
      label: "Rules",
      model: "sale.order",
      value: initial(),
      fields: () => fields,
      change: vi.fn(),
    })
  ).toThrowError(
    expect.objectContaining({
      kind: "validation",
      message: "Unknown tree editor option: unexpected.",
    }),
  );
});
test("new condition skips unsupported field kinds without inventing a path or default", async () => {
  const metadata = new Map([["attachment", field("attachment", "binary")], [
    "name",
    field("name"),
  ]]);
  const change = vi.fn();
  const editor = createTreeEditor({
    environment: env(),
    label: "Rules",
    model: "sale.order",
    value: { id: "root", kind: "and", children: [] },
    fields: () => metadata,
    change,
  });
  handles.push(editor);
  document.body.append(editor.node);
  await editor.ready;
  control<HTMLButtonElement>(
    editor.node,
    '[data-tree-node="root"] > .editor-tree-actions [data-tree-action="add"]',
  ).click();
  await vi.waitFor(() => expect(change).toHaveBeenCalledTimes(1));
  expect(change.mock.calls[0]![0]).toMatchObject({
    children: [{
      kind: "condition",
      path: "name",
      operator: "=",
      value: { kind: "string", value: "" },
    }],
  });
});
test("a single root condition can add a sibling without a preliminary nesting edit", async () => {
  const change = vi.fn();
  const editor = createTreeEditor({
    environment: env(),
    label: "Rules",
    model: "sale.order",
    value: {
      id: "single",
      kind: "condition",
      path: "name",
      operator: "=",
      value: { kind: "string", value: "A" },
    },
    fields: () => fields,
    change,
  });
  handles.push(editor);
  document.body.append(editor.node);
  await editor.ready;
  const add = editor.node.querySelector<HTMLButtonElement>(
    '[data-tree-node="single"] > .editor-tree-actions [data-tree-action="add"]',
  );
  expect(add).toBeInstanceOf(HTMLButtonElement);
  add!.click();
  await vi.waitFor(() => expect(change).toHaveBeenCalledTimes(1));
  expect(change.mock.calls[0]![0]).toMatchObject({
    kind: "and",
    children: [{
      id: "single",
      path: "name",
      value: { kind: "string", value: "A" },
    }, { kind: "condition", path: "name" }],
  });
});
test("tree action toolbars use one tab stop with Arrow, Home and End navigation", async () => {
  const { editor } = await fixture();
  const toolbar = control<HTMLElement>(
    editor.node,
    '[data-tree-node="root"] > .editor-tree-actions',
  );
  expect(toolbar.getAttribute("role")).toBe("toolbar");
  expect(toolbar.getAttribute("aria-label")).toBe("Actions for rule 1");
  const buttons = [...toolbar.querySelectorAll<HTMLButtonElement>("button")];
  expect(buttons.filter((button) => button.tabIndex === 0)).toHaveLength(1);
  buttons[0]!.focus();
  buttons[0]!.dispatchEvent(
    new KeyboardEvent("keydown", { key: "ArrowRight", bubbles: true }),
  );
  expect(document.activeElement).toBe(buttons[1]);
  buttons[1]!.dispatchEvent(
    new KeyboardEvent("keydown", { key: "End", bubbles: true }),
  );
  expect(document.activeElement).toBe(buttons.at(-1));
  buttons.at(-1)!.dispatchEvent(
    new KeyboardEvent("keydown", { key: "Home", bubbles: true }),
  );
  expect(document.activeElement).toBe(buttons[0]);
  expect(buttons.filter((button) => button.tabIndex === 0)).toHaveLength(1);
  await expect(auditComponent(editor.node)).resolves.toBeDefined();
});
test("asynchronous tree commits preserve focus moved to a live external control", async () => {
  let release!: () => void;
  const change = vi.fn(() =>
    new Promise<void>((resolve) => {
      release = resolve;
    })
  );
  const { editor } = await fixture(change);
  const external = document.createElement("button");
  external.textContent = "Outside";
  document.body.append(external);
  const input = control<HTMLInputElement>(
    editor.node,
    '[data-tree-node="a"] input[data-tree-value]',
  );
  input.focus();
  input.value = "Draft";
  input.dispatchEvent(new Event("change"));
  await vi.waitFor(() => expect(change).toHaveBeenCalledTimes(1));
  external.focus();
  release();
  // The original input already contains Draft. Wait for the represented tree's
  // replacement control and the whole commit to settle before checking focus.
  await vi.waitFor(() => {
    expect(editor.node.getAttribute("aria-busy")).toBe("false");
    expect(
      control<HTMLInputElement>(
        editor.node,
        '[data-tree-node="a"] input[data-tree-value]',
      ),
    ).not.toBe(input);
  });
  await vi.waitFor(() =>
    expect(
      control<HTMLInputElement>(
        editor.node,
        '[data-tree-node="a"] input[data-tree-value]',
      ).value,
    ).toBe("Draft")
  );
  expect(document.activeElement).toBe(external);
});
test("condition rows announce pending metadata and clear it when controls are ready", async () => {
  let release!: (value: ReadonlyMap<string, Field>) => void;
  const pending = new Promise<ReadonlyMap<string, Field>>((resolve) => {
    release = resolve;
  });
  const editor = createTreeEditor({
    environment: env(),
    label: "Rules",
    model: "sale.order",
    value: initial(),
    fields: () => pending,
    change: vi.fn(),
  });
  handles.push(editor);
  document.body.append(editor.node);
  const row = control<HTMLElement>(editor.node, '[data-tree-node="a"]');
  expect(row.querySelector('[role="status"]')?.textContent).toBe(
    "Loading field metadata…",
  );
  expect(row.querySelector(".editor-tree-controls")?.getAttribute("aria-busy"))
    .toBe("true");
  release(fields);
  await editor.ready;
  expect(row.textContent).not.toContain("Loading field metadata…");
  expect(row.querySelector(".editor-tree-controls")?.getAttribute("aria-busy"))
    .toBe("false");
  expect(row.querySelector("[data-field-open]")).not.toBeNull();
});
test("owner replacement retains the pending callback lock until the previous write settles", async () => {
  let release!: () => void;
  let calls = 0;
  const pending = new Promise<void>((resolve) => {
    release = resolve;
  });
  const change = vi.fn<(value: ConditionNode) => void | Promise<void>>(() => {
    calls++;
    if (calls === 1) return pending;
    return undefined;
  });
  const { editor } = await fixture(change);
  const first = control<HTMLInputElement>(
    editor.node,
    '[data-tree-node="a"] input[data-tree-value]',
  );
  first.value = "First draft";
  first.dispatchEvent(new Event("change"));
  await vi.waitFor(() => expect(change).toHaveBeenCalledTimes(1));
  await editor.setValue({
    id: "owner",
    kind: "condition",
    path: "name",
    operator: "=",
    value: { kind: "string", value: "Owner replacement" },
  });
  const replacement = control<HTMLInputElement>(
    editor.node,
    '[data-tree-node="owner"] input[data-tree-value]',
  );
  expect(control<HTMLElement>(editor.node, ".editor-tree-body").inert).toBe(true);
  expect(replacement.disabled).toBe(false);
  expect(editor.node.getAttribute("aria-busy")).toBe("true");
  replacement.value = "Must wait";
  replacement.dispatchEvent(new Event("change"));
  await Promise.resolve();
  await Promise.resolve();
  expect(change).toHaveBeenCalledTimes(1);
  release();
  await vi.waitFor(() =>
    expect(editor.node.getAttribute("aria-busy")).toBe("false")
  );
  const after = control<HTMLInputElement>(
    editor.node,
    '[data-tree-node="owner"] input[data-tree-value]',
  );
  expect(after.disabled).toBe(false);
  expect(editor.node.querySelector('[data-tree-node="a"]')).toBeNull();
  after.value = "After settlement";
  after.dispatchEvent(new Event("change"));
  await vi.waitFor(() => expect(change).toHaveBeenCalledTimes(2));
  expect(change.mock.calls[1]![0]).toMatchObject({
    id: "owner",
    value: { kind: "string", value: "After settlement" },
  });
});


test("tree edits preserve provider-owned disabled controls after success, rejection and owner replacement", async () => {
  for (const outcome of ["success", "rejection", "replacement"] as const) {
    let release!: () => void;
    let reject!: (error: Error) => void;
    const pending = new Promise<void>((resolve, refuse) => { release = resolve; reject = refuse; });
    const environment = env(), change = vi.fn(() => pending);
    const editor = createTreeEditor({
      environment, label: "Rules", model: "sale.order", value: initial(), fields: () => fields, change,
      provider: { ...defaultTreeEditorProvider("domain"), valueEditor() {
        const node = document.createElement("select");
        node.dataset["providerLocked"] = "";
        node.disabled = true;
        return { node, dispose() {} };
      } },
    });
    handles.push(editor); document.body.append(editor.node); await editor.ready;
    expect(control<HTMLSelectElement>(editor.node, "[data-provider-locked]").disabled).toBe(true);
    control<HTMLButtonElement>(editor.node, '[data-tree-node="a"] [data-tree-action="nest"]').click();
    await vi.waitFor(() => expect(change).toHaveBeenCalledTimes(1));
    if (outcome === "replacement") await editor.setValue(initial());
    if (outcome === "rejection") reject(new Error("Owner refused tree")); else release();
    await vi.waitFor(() => expect(editor.node.getAttribute("aria-busy")).toBe("false"));
    expect(control<HTMLSelectElement>(editor.node, "[data-provider-locked]").disabled).toBe(true);
    expect(control<HTMLElement>(editor.node, ".editor-tree-body").inert).toBe(false);
    expect(control<HTMLButtonElement>(editor.node, '[data-tree-action="add"]').disabled).toBe(false);
    expect(environment.report).toHaveBeenCalledTimes(outcome === "rejection" ? 1 : 0);
    editor.dispose(); editor.node.remove();
  }
});

test("a provider edit rejects while a tree callback is pending and can edit after settlement", async () => {
  const callbacks: TreeValueEditorOptions["change"][] = [];
  let release!: () => void;
  const pending = new Promise<void>(resolve => { release = resolve; });
  const change = vi.fn<(value: ConditionNode) => void | Promise<void>>().mockImplementationOnce(() => pending);
  const environment = env();
  const editor = createTreeEditor({
    environment, label: "Rules", model: "sale.order", value: initial(), fields: () => fields, change,
    provider: { ...defaultTreeEditorProvider("domain"), valueEditor(options) {
      callbacks.push(options.change);
      return { node: document.createElement("input"), dispose() {} };
    } },
  });
  handles.push(editor); document.body.append(editor.node); await editor.ready;
  const first = callbacks[0]!({kind: "string", value: "First"});
  await vi.waitFor(() => expect(change).toHaveBeenCalledTimes(1));
  await expect(callbacks[0]!({kind: "string", value: "Second"})).rejects.toMatchObject({
    kind: "validation", message: "A tree change is already pending.",
  });
  expect(change).toHaveBeenCalledTimes(1);
  // Synthetic events can bypass inert. Their event boundary must report the refusal.
  const connector = control<HTMLSelectElement>(editor.node, "[data-tree-connector]");
  connector.value = "or"; connector.dispatchEvent(new Event("change"));
  await vi.waitFor(() => expect(environment.report).toHaveBeenCalledTimes(1));
  expect(environment.report.mock.calls[0]![0]).toMatchObject({kind: "validation", message: "A tree change is already pending."});
  release(); await first;
  await callbacks[callbacks.length - 1]!({kind: "string", value: "Third"});
  expect(change).toHaveBeenCalledTimes(2);
  expect(change.mock.calls[1]![0]).toMatchObject({children: [{value: {kind: "string", value: "Third"}}]});
  expect(environment.report).toHaveBeenCalledTimes(1);
});

test("owner replacement clears interaction suppression from obsolete metadata preparation", async () => {
  let release!: (value: ReadonlyMap<string, Field>) => void;
  const pending = new Promise<ReadonlyMap<string, Field>>(resolve => { release = resolve; });
  let preparing = false;
  const metadata = vi.fn(() => preparing ? pending : fields);
  const change = vi.fn(), editor = createTreeEditor({
    environment: env(), label: "Rules", model: "sale.order", value: initial(), fields: metadata, change,
  });
  handles.push(editor); document.body.append(editor.node); await editor.ready;
  const before = metadata.mock.calls.length;
  preparing = true;
  control<HTMLButtonElement>(editor.node, '[data-tree-node="root"] > .editor-tree-actions [data-tree-action="add"]').click();
  await vi.waitFor(() => expect(metadata).toHaveBeenCalledTimes(before + 1));
  await editor.setValue({ id: "owner", kind: "constant", value: true });
  expect(editor.node.getAttribute("aria-busy")).toBe("false");
  expect(control<HTMLElement>(editor.node, ".editor-tree-body").inert).toBe(false);
  release(fields);
  await Promise.resolve(); await Promise.resolve();
  expect(change).not.toHaveBeenCalled();
});
