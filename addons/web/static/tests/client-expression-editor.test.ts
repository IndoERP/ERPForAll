/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { MessageServices } from "../src/client/messages.js";
import {
  createExpressionEditor,
  type ExpressionField,
} from "../src/client/expression-editor.js";
let messages: MessageServices | undefined;
beforeEach(() => {
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
  messages?.dispose();
  document.body.replaceChildren();
});
function environment() {
  messages?.dispose();
  messages = new MessageServices(document.body, vi.fn());
  return { messages, report: vi.fn() };
}
const fields = (_model?: string, _signal?: AbortSignal) =>
  new Map<string, ExpressionField>([
    ["name", {
      name: "name",
      label: "Name",
      kind: "char",
      required: false,
      readonly: false,
      relation: null,
      relationField: null,
      selection: null,
    }],
    ["partner_id", {
      name: "partner_id",
      label: "Contact",
      kind: "many2one",
      required: false,
      readonly: false,
      relation: "res.partner",
      relationField: null,
      selection: null,
    }],
    ["properties", {
      name: "properties",
      label: "Properties",
      kind: "properties",
    }],
  ]);
const raw = (node: HTMLElement) =>
  node.querySelector<HTMLTextAreaElement>("[data-editor-source]")!;
const reset = (node: HTMLElement) =>
  node.querySelector<HTMLButtonElement>("[data-editor-reset]")!;
/// (partial: raw status, explicit True reset, properties omission and no relation traversal)
test("expression metadata excludes properties and never offers related traversal", async () => {
  const metadata = vi.fn(fields), change = vi.fn();
  const editor = createExpressionEditor({
    environment: environment(),
    label: "Visible when",
    model: "sale.order",
    value: "name == 'A'",
    fields: metadata,
    change,
  });
  document.body.append(editor.node);
  await editor.ready;
  editor.node.querySelector<HTMLButtonElement>("[data-field-open]")!.click();
  await vi.waitFor(() =>
    expect(document.querySelector("[data-field-name=name]")).not.toBeNull()
  );
  expect(document.querySelector("[data-field-name=properties]")).toBeNull();
  expect(document.querySelector(".editor-field-follow")).toBeNull();
  expect(metadata.mock.calls.every(([model]) => model === "sale.order")).toBe(
    true,
  );
  expect(change).not.toHaveBeenCalled();
  editor.dispose();
});
test("expression unsupported paths remain raw and are never added to field choices", async () => {
  const editor = createExpressionEditor({
    environment: environment(),
    label: "Visible when",
    model: "sale.order",
    value: "partner_id.name",
    fields,
    debug: true,
    change: vi.fn(),
  });
  await editor.ready;
  expect(editor.node.dataset["state"]).toBe("unsupported");
  expect(raw(editor.node).value).toBe("partner_id.name");
  expect(editor.node.querySelector('[data-field-name="partner_id.name"]'))
    .toBeNull();
  editor.dispose();
});
test("expression raw changes carry exact invalid status and reset emits True once", async () => {
  const change = vi.fn(),
    editor = createExpressionEditor({
      environment: environment(),
      label: "Visible when",
      model: "sale.order",
      value: "True",
      fields,
      debug: true,
      change,
    });
  await editor.ready;
  raw(editor.node).value = "name ==\n";
  raw(editor.node).dispatchEvent(new Event("input"));
  await vi.waitFor(() => expect(change).toHaveBeenCalledTimes(1));
  expect(change.mock.calls[0]![0]).toMatchObject({
    source: "name ==\n",
    status: "invalid",
    error: { kind: "validation", message: "Expected a literal." },
  });
  await vi.waitFor(() =>
    expect(editor.node.getAttribute("aria-busy")).toBe("false")
  );
  reset(editor.node).click();
  await vi.waitFor(() => expect(change).toHaveBeenCalledTimes(2));
  expect(change.mock.calls[1]![0]).toMatchObject({
    source: "True",
    status: "supported",
  });
  editor.dispose();
});
test("expression readonly debug raw is readonly and has no reset", async () => {
  const change = vi.fn(),
    editor = createExpressionEditor({
      environment: environment(),
      label: "Visible when",
      model: "sale.order",
      value: "True",
      fields,
      readonly: true,
      debug: true,
      change,
    });
  await editor.ready;
  expect(raw(editor.node).readOnly).toBe(true);
  expect(reset(editor.node)).toBeNull();
  raw(editor.node).dispatchEvent(new Event("input"));
  expect(change).not.toHaveBeenCalled();
  editor.dispose();
});
test("expression metadata replacements suppress obsolete completion and callbacks after disposal", async () => {
  let release!: (value: ReturnType<typeof fields>) => void;
  let first = true;
  const metadata = vi.fn(() => {
    if (first) {
      first = false;
      return new Promise<ReturnType<typeof fields>>((resolve) => {
        release = resolve;
      });
    }
    return fields();
  });
  const change = vi.fn(),
    editor = createExpressionEditor({
      environment: environment(),
      label: "Visible when",
      model: "sale.order",
      value: "name",
      fields: metadata,
      debug: true,
      change,
    });
  await editor.setValue("True");
  release(fields());
  await editor.ready;
  expect(raw(editor.node).value).toBe("True");
  expect(change).not.toHaveBeenCalled();
  editor.dispose();
  raw(editor.node).value = "False";
  raw(editor.node).dispatchEvent(new Event("input"));
  expect(change).not.toHaveBeenCalled();
});
test("expression source authoring neither evaluates nor makes network requests", async () => {
  const evaluate = vi.spyOn(globalThis, "eval"),
    network = vi.spyOn(globalThis, "fetch");
  try {
    const editor = createExpressionEditor({
      environment: environment(),
      label: "Visible when",
      model: "sale.order",
      value: "fetch('/draft')",
      fields,
      debug: true,
      change: vi.fn(),
    });
    await editor.ready;
    expect(editor.node.dataset["state"]).toBe("unsupported");
    expect(evaluate).not.toHaveBeenCalled();
    expect(network).not.toHaveBeenCalled();
    editor.dispose();
  } finally {
    evaluate.mockRestore();
    network.mockRestore();
  }
});
test("expression authoring excludes properties and traversal while emitting tree and raw edits with explicit recovery", async () => {
  const change = vi.fn(),
    metadata = vi.fn(fields),
    editor = createExpressionEditor({
      environment: environment(),
      label: "Visible when",
      model: "sale.order",
      value: "name == 'A'",
      fields: metadata,
      debug: true,
      change,
    });
  document.body.append(editor.node);
  await editor.ready;
  editor.node.querySelector<HTMLButtonElement>("[data-field-open]")!.click();
  await vi.waitFor(() =>
    expect(document.querySelector("[data-field-name=name]")).not.toBeNull()
  );
  expect(document.querySelector("[data-field-name=properties]")).toBeNull();
  expect(document.querySelector(".editor-field-follow")).toBeNull();
  document.querySelector<HTMLElement>("[data-field-name=name]")!.dispatchEvent(
    new Event("click"),
  );
  await vi.waitFor(() => expect(change).toHaveBeenCalledTimes(1));
  await vi.waitFor(() =>
    expect(editor.node.querySelector(".editor-tree")?.getAttribute("aria-busy"))
      .toBe("false")
  );
  const input = editor.node.querySelector<HTMLInputElement>(
    "[data-tree-value]",
  )!;
  input.value = "B";
  input.dispatchEvent(new Event("change"));
  await vi.waitFor(() => expect(change).toHaveBeenCalledTimes(2));
  expect(change.mock.calls[1]![0].source).toBe("name == 'B'");
  await editor.setValue("partner_id.name");
  expect(editor.node.dataset["state"]).toBe("unsupported");
  expect(editor.node.querySelector('[data-field-name="partner_id.name"]'))
    .toBeNull();
  raw(editor.node).value = "name ==\n";
  raw(editor.node).dispatchEvent(new Event("input"));
  await vi.waitFor(() => expect(change).toHaveBeenCalledTimes(3));
  expect(change.mock.calls[2]![0]).toMatchObject({
    source: "name ==\n",
    status: "invalid",
  });
  await vi.waitFor(() =>
    expect(editor.node.getAttribute("aria-busy")).toBe("false")
  );
  reset(editor.node).click();
  await vi.waitFor(() => expect(change).toHaveBeenCalledTimes(4));
  expect(change.mock.calls[3]![0].source).toBe("True");
  expect(metadata.mock.calls.every(([model]) => model === "sale.order")).toBe(
    true,
  );
  editor.dispose();
});
test("bare expression fields still validate metadata policies before callbacks", async () => {
  const env = environment(), change = vi.fn(), metadata = fields();
  const name = metadata.get("name")!;
  // @ts-expect-error Runtime metadata policies require booleans even for bare fields.
  metadata.set("name", { ...name, kind: "char", required: "yes" });
  const editor = createExpressionEditor({
    environment: env,
    label: "Visible when",
    model: "sale.order",
    value: "name",
    fields: () => metadata,
    debug: true,
    change,
  });
  await editor.ready;
  expect(editor.node.dataset["state"]).toBe("error");
  expect(env.report).toHaveBeenCalledWith(
    expect.objectContaining({
      kind: "validation",
      message: "Invalid field metadata policy.",
    }),
  );
  expect(change).not.toHaveBeenCalled();
  editor.dispose();
});

test("expression raw drafts stay editable and publish only the latest pending source", async () => {
  const releases: (() => void)[] = [], env = environment();
  const change = vi.fn((_result: { source: string }) => new Promise<void>((resolve) => releases.push(resolve)));
  const editor = createExpressionEditor({ environment: env, label: "Visible when", model: "sale.order", value: "True", fields, debug: true, change });
  document.body.append(editor.node);
  await editor.ready;
  const textarea = raw(editor.node);
  const input = (source: string) => { textarea.value = source; textarea.dispatchEvent(new Event("input", { bubbles: true })); };
  input("name == 'A'");
  await vi.waitFor(() => expect(change).toHaveBeenCalledTimes(1));
  expect(textarea.readOnly).toBe(false);
  input("name == 'B'"); input("name == 'C'");
  releases[0]!();
  await vi.waitFor(() => expect(change).toHaveBeenCalledTimes(2));
  expect(change.mock.calls.map(([result]) => result.source)).toEqual(["name == 'A'", "name == 'C'"]);
  expect(raw(editor.node)).toBe(textarea);
  expect(textarea.value).toBe("name == 'C'");
  releases[1]!();
  await vi.waitFor(() => expect(editor.node.getAttribute("aria-busy")).toBe("false"));
  expect(env.report).not.toHaveBeenCalled();
  editor.dispose();
});
test("rejected expression tree edits have one owning alert and one report", async () => {
  const env = environment(), failure = new Error("Owner refused expression tree"), change = vi.fn().mockRejectedValue(failure);
  const editor = createExpressionEditor({ environment: env, label: "Visible when", model: "sale.order", value: "name == 'A'", fields, debug: true, change });
  document.body.append(editor.node);
  await editor.ready;
  editor.node.querySelector<HTMLButtonElement>('[data-tree-action="nest"]')!.click();
  await vi.waitFor(() => expect(env.report).toHaveBeenCalled());
  expect(env.report).toHaveBeenCalledExactlyOnceWith(failure);
  expect(editor.node.querySelectorAll('[role="alert"]')).toHaveLength(1);
  expect(raw(editor.node).value).toBe("name == 'A'");
  expect(change).toHaveBeenCalledTimes(1);
  editor.dispose();
});
