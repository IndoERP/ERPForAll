/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { createTreeValueEditor } from "../src/client/tree-editor-values.js";
import { defaultTreeEditorProvider } from "../src/client/tree-editor-fields.js";
import type { ConditionLiteral } from "../src/client/condition-tree.js";
import type { Field } from "../src/client/metadata.js";
import type { EditorHandle } from "../src/client/editor-types.js";
import { MessageServices } from "../src/client/messages.js";
let messages: MessageServices;
const handles: EditorHandle[] = [];
const field = (kind: Field["kind"]): Field => ({
  name: "value",
  kind,
  label: "Value",
  required: false,
  readonly: false,
  selection: null,
  relation: kind === "many2one" ? "res.partner" : null,
  relationField: null,
});
beforeEach(() => {
  messages = new MessageServices(document.body, vi.fn());
});
afterEach(() => {
  handles.splice(0).forEach((handle) => handle.dispose());
  messages.dispose();
  document.body.replaceChildren();
});
function fixture(kind: Field["kind"], value: ConditionLiteral, operator = "=") {
  const environment = { messages, report: vi.fn() }, change = vi.fn();
  const editor = createTreeValueEditor({
    environment,
    field: field(kind),
    label: "Condition value",
    value,
    operator,
    change,
  });
  handles.push(editor);
  document.body.append(editor.node);
  return { editor, change, environment };
}
function input(editor: EditorHandle): HTMLInputElement | HTMLTextAreaElement {
  const control = editor.node.querySelector<
    HTMLInputElement | HTMLTextAreaElement
  >("[data-tree-value]");
  if (!control) throw new Error("Missing value input");
  return control;
}
/// (partial: typed default literal controls and exact integer identities)
test("integer and relation identities preserve the complete signed i64 range", async () => {
  for (const kind of ["integer", "many2one"] as const) {
    const { editor, change } = fixture(kind, { kind: "integer", value: "0" });
    const control = input(editor);
    control.value = "9223372036854775807";
    control.dispatchEvent(new Event("change"));
    await vi.waitFor(() =>
      expect(change).toHaveBeenCalledWith({
        kind: "integer",
        value: "9223372036854775807",
      })
    );
    control.value = "-9223372036854775808";
    control.dispatchEvent(new Event("change"));
    await vi.waitFor(() =>
      expect(change).toHaveBeenCalledWith({
        kind: "integer",
        value: "-9223372036854775808",
      })
    );
  }
});
test("integer overflow and nonfinite float drafts remain visible and never emit", async () => {
  for (
    const [kind, source] of [["integer", "9223372036854775808"], [
      "float",
      "1e999",
    ]] as const
  ) {
    const { editor, change, environment } = fixture(
      kind,
      kind === "integer"
        ? { kind: "integer", value: "0" }
        : { kind: "float", value: 0 },
    );
    const control = input(editor);
    control.value = source;
    control.dispatchEvent(new Event("change"));
    await vi.waitFor(() => expect(environment.report).toHaveBeenCalled());
    expect(change).not.toHaveBeenCalled();
    expect(control.value).toBe(source);
    expect(control.getAttribute("aria-invalid")).toBe("true");
    expect(editor.node.querySelector('[role="alert"]')?.textContent).not.toBe(
      "",
    );
  }
});
test("string editing is literal text and membership accepts only literal lists or tuples", async () => {
  const text = fixture("char", { kind: "string", value: "" });
  input(text.editor).value = "<img src=x onerror=alert(1)>";
  input(text.editor).dispatchEvent(new Event("change"));
  await vi.waitFor(() =>
    expect(text.change).toHaveBeenCalledWith({
      kind: "string",
      value: "<img src=x onerror=alert(1)>",
    })
  );
  expect(text.editor.node.querySelector("img")).toBeNull();
  const members = fixture("integer", { kind: "list", items: [] }, "in");
  input(members.editor).value = "[0, 9223372036854775807]";
  input(members.editor).dispatchEvent(new Event("change"));
  await vi.waitFor(() =>
    expect(members.change).toHaveBeenCalledWith({
      kind: "list",
      items: [{ kind: "integer", value: "0" }, {
        kind: "integer",
        value: "9223372036854775807",
      }],
    })
  );
  members.change.mockClear();
  input(members.editor).value = "0";
  input(members.editor).dispatchEvent(new Event("change"));
  await vi.waitFor(() =>
    expect(members.environment.report).toHaveBeenCalledWith(
      expect.objectContaining({
        kind: "validation",
        message: "Membership values must be a list or tuple.",
      }),
    )
  );
  expect(members.change).not.toHaveBeenCalled();
});
test("boolean values stay booleans and default policy never treats zero as false", () => {
  const provider = defaultTreeEditorProvider("domain");
  expect(
    provider.accepts(field("integer"), "=", { kind: "boolean", value: false }),
  ).toBe(false);
  expect(
    provider.accepts(field("integer"), "=", { kind: "integer", value: "0" }),
  ).toBe(true);
  expect(provider.initialValue(field("boolean"), "=")).toEqual({
    kind: "boolean",
    value: false,
  });
  expect(provider.operators(field("integer")).map((item) => item.value))
    .toContain(">=");
  expect(provider.operators(field("boolean")).map((item) => item.value)).not
    .toContain("ilike");
});
test("unknown kinds and invalid policy results refuse rather than choose an invented default", () => {
  const provider = defaultTreeEditorProvider("domain");
  expect(() => provider.initialValue(field("binary"), "=")).toThrowError(
    expect.objectContaining({
      kind: "unsupported",
      message:
        "Binary and collection condition values require a custom tree editor provider.",
    }),
  );
});
test("disposed value controls do not emit detached edits", () => {
  const { editor, change } = fixture("char", { kind: "string", value: "old" });
  const control = input(editor);
  editor.dispose();
  editor.dispose();
  control.value = "detached";
  control.dispatchEvent(new Event("change"));
  expect(change).not.toHaveBeenCalled();
});
test("numeric float controls preserve authored large integer literal identity", async () => {
  const { editor, change } = fixture("float", { kind: "float", value: 0 });
  input(editor).value = "9223372036854775807";
  input(editor).dispatchEvent(new Event("change"));
  await vi.waitFor(() =>
    expect(change).toHaveBeenCalledWith({
      kind: "integer",
      value: "9223372036854775807",
    })
  );
});
test("float metadata permits fractional edits after retaining an integer literal", async () => {
  const { editor, change } = fixture("float", {
    kind: "integer",
    value: "9223372036854775807",
  });
  input(editor).value = "1.5";
  input(editor).dispatchEvent(new Event("change"));
  await vi.waitFor(() =>
    expect(change).toHaveBeenCalledWith({ kind: "float", value: 1.5 })
  );
});
