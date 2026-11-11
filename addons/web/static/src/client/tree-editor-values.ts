/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { el, errorMessage } from "./dom.js";
import {
  editorEnvironment,
  type EditorHandle,
  editorOptions,
  editorText,
} from "./editor-types.js";
import {
  parseConditionLiteral,
  serializeConditionLiteral,
} from "./condition-literals.js";
import type { ConditionLiteral } from "./condition-tree.js";
import type { TreeValueEditorOptions } from "./tree-editor-fields.js";
import { ClientError } from "./transport.js";
/** Default bounded literal control for a condition value; failed drafts never emit. */
export function createTreeValueEditor(
  options: TreeValueEditorOptions,
): EditorHandle {
  editorOptions(options, [
    "environment",
    "field",
    "operator",
    "value",
    "label",
    "limits",
    "change",
  ], "Tree value editor");
  editorEnvironment(options.environment);
  editorText(options.label, "Tree value label");
  if (typeof options.change !== "function") {
    throw new ClientError(
      "validation",
      "Tree value change must be a function.",
    );
  }
  serializeConditionLiteral(options.value, options.limits);
  const node = el("div", undefined, "editor-tree-value"),
    status = el("span", undefined, "editor-tree-status");
  status.setAttribute("role", "status");
  const member = options.operator === "in" || options.operator === "not in";
  const identity = options.operator === "is" || options.operator === "is not";
  const boolean = options.value.kind === "boolean" && !member;
  const text = options.value.kind === "string" && !member;
  const control = boolean
    ? el("select")
    : member
    ? el("textarea")
    : el("input");
  control.dataset["treeValue"] = "";
  control.setAttribute("aria-label", options.label);
  control.setAttribute("aria-invalid", "false");
  if (boolean) {
    for (const value of ["True", "False"]) {
      const option = el("option", value);
      option.value = value;
      control.append(option);
    }
  }
  control.value = options.value.kind === "string" && !member
    ? options.value.value
    : serializeConditionLiteral(options.value, options.limits);
  if (control instanceof HTMLInputElement) {
    control.type = "text";
    if (
      ["integer", "float", "monetary", "many2one"].includes(options.field.kind)
    ) control.inputMode = "decimal";
  }
  node.append(control, status);
  let dead = false, pending = false;
  const commit = async (): Promise<void> => {
    if (dead || pending || options.environment.signal?.aborted) return;
    pending = true;
    control.disabled = true;
    try {
      editorText(
        control.value,
        "Condition value",
        options.limits?.sourceBytes ?? 65536,
      );
      let value: ConditionLiteral;
      if (member) {
        value = parseConditionLiteral(control.value, options.limits);
        if (value.kind !== "list" && value.kind !== "tuple") {
          throw new ClientError(
            "validation",
            "Membership values must be a list or tuple.",
          );
        }
      } else if (text) value = { kind: "string", value: control.value };
      else {
        value = parseConditionLiteral(control.value, options.limits);
        if (boolean && value.kind !== "boolean") {
          throw new ClientError(
            "validation",
            "Boolean condition value must be True or False.",
          );
        }
        if (identity && value.kind !== "none" && value.kind !== "boolean") {
          throw new ClientError(
            "validation",
            "Identity condition value must be None, True or False.",
          );
        }
        if (
          !identity &&
          (options.field.kind === "integer" ||
            options.field.kind === "many2one") &&
          value.kind !== "integer"
        ) {
          throw new ClientError(
            "validation",
            "Integer condition value must be a signed i64 integer.",
          );
        }
        if (
          !identity &&
          (options.field.kind === "float" || options.field.kind === "monetary")
        ) {
          if (value.kind !== "float" && value.kind !== "integer") {
            throw new ClientError(
              "validation",
              "Numeric condition value must be finite.",
            );
          }
        }
      }
      serializeConditionLiteral(value, options.limits);
      await options.change(value);
      if (!dead) {
        status.textContent = "";
        control.setAttribute("aria-invalid", "false");
      }
    } catch (error) {
      if (!dead && !options.environment.signal?.aborted) {
        status.setAttribute("role", "alert");
        status.textContent = errorMessage(error).slice(0, 1024);
        control.setAttribute("aria-invalid", "true");
        options.environment.report(error);
      }
    } finally {
      pending = false;
      if (!dead) control.disabled = false;
    }
  };
  control.addEventListener("change", () => {
    void commit();
  });
  const dispose = (): void => {
    if (dead) return;
    dead = true;
    options.environment.signal?.removeEventListener("abort", dispose);
  };
  options.environment.signal?.addEventListener("abort", dispose, {
    once: true,
  });
  if (options.environment.signal?.aborted) dispose();
  return { node, dispose };
}
