/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { ConditionLiteral, TreeLimits } from "./condition-tree.js";
import type { EditorEnvironment, EditorHandle } from "./editor-types.js";
import type { Field } from "./metadata.js";
import { editorText } from "./editor-types.js";
import { ClientError } from "./transport.js";
import { serializeConditionLiteral } from "./condition-literals.js";
export interface TreeEditorOperator {
  readonly value: string;
  readonly label: string;
}
export interface TreeValueEditorOptions {
  environment: EditorEnvironment;
  field: Field;
  operator: string;
  value: ConditionLiteral;
  label: string;
  limits?: TreeLimits;
  change(value: ConditionLiteral): void | Promise<void>;
}
/** Trusted owner callbacks customize authoring only; they grant no execution authority. */
export interface TreeEditorProvider {
  operators(field: Field): readonly TreeEditorOperator[];
  initialValue(field: Field, operator: string): ConditionLiteral;
  accepts(field: Field, operator: string, value: ConditionLiteral): boolean;
  describe(field: Field, operator: string, value: ConditionLiteral): string;
  valueEditor?(options: TreeValueEditorOptions): EditorHandle;
}
const membership = (operator: string): boolean =>
  operator === "in" || operator === "not in";
const numeric = (field: Field): boolean =>
  ["integer", "many2one", "float", "monetary"].includes(field.kind);
const stringKind = (field: Field): boolean =>
  ["char", "text", "date", "datetime", "selection"].includes(field.kind);
const unsupportedMessage =
  "Binary and collection condition values require a custom tree editor provider (a later release advanced controls).";
/** Default policies are conservative; relation identities stay checked integer literals. */
export function defaultTreeEditorProvider(
  mode: "domain" | "expression",
): TreeEditorProvider {
  if (mode !== "domain" && mode !== "expression") {
    throw new ClientError("validation", "Invalid tree editor mode.");
  }
  const scalar = (field: Field, value: ConditionLiteral): boolean => {
    if (field.kind === "boolean") return value.kind === "boolean";
    if (field.kind === "integer" || field.kind === "many2one") {
      return value.kind === "integer";
    }
    if (field.kind === "float" || field.kind === "monetary") {
      return value.kind === "float" || value.kind === "integer";
    }
    if (stringKind(field)) {
      return value.kind === "string" &&
        (field.kind !== "selection" ||
          field.selection?.some(([key]) => key === value.value) === true);
    }
    return false;
  };
  const operators = (field: Field): readonly TreeEditorOperator[] => {
    // An empty choice list explicitly excludes this kind from automatic creation.
    if (!numeric(field) && !stringKind(field) && field.kind !== "boolean") {
      return [];
    }
    const values = [mode === "domain" ? "=" : "==", "!=", "in", "not in"];
    if (numeric(field) || field.kind === "date" || field.kind === "datetime") {
      values.push("<", "<=", ">", ">=");
    }
    if (mode === "domain" && ["char", "text"].includes(field.kind)) {
      values.push(
        "like",
        "not like",
        "ilike",
        "not ilike",
        "=like",
        "not =like",
        "=ilike",
        "not =ilike",
      );
    }
    if (mode === "domain") values.push("=?");
    if (mode === "expression") values.push("is", "is not");
    return values.map((value) => ({ value, label: value }));
  };
  const initialValue = (field: Field, operator: string): ConditionLiteral => {
    if (!operators(field).some((choice) => choice.value === operator)) {
      throw new ClientError("unsupported", unsupportedMessage);
    }
    if (membership(operator)) return { kind: "list", items: [] };
    if (operator === "is" || operator === "is not") return { kind: "none" };
    if (field.kind === "boolean") return { kind: "boolean", value: false };
    if (field.kind === "integer" || field.kind === "many2one") {
      return { kind: "integer", value: "0" };
    }
    if (field.kind === "float" || field.kind === "monetary") {
      return { kind: "float", value: 0 };
    }
    if (field.kind === "selection") {
      const first = field.selection?.[0];
      if (!first) {
        throw new ClientError(
          "validation",
          "Selection condition field has no available choices.",
        );
      }
      return { kind: "string", value: first[0] };
    }
    if (stringKind(field)) return { kind: "string", value: "" };
    throw new ClientError("unsupported", unsupportedMessage);
  };
  const accepts = (
    field: Field,
    operator: string,
    value: ConditionLiteral,
  ): boolean => {
    if (!operators(field).some((item) => item.value === operator)) return false;
    if (operator === "is" || operator === "is not") {
      return value.kind === "none" || value.kind === "boolean";
    }
    if (membership(operator)) {
      return (value.kind === "list" || value.kind === "tuple") &&
        value.items.every((item) => scalar(field, item));
    }
    return scalar(field, value);
  };
  return {
    operators,
    initialValue,
    accepts,
    describe: (field, operator, value) =>
      `${field.label} ${operator} ${serializeConditionLiteral(value)}`,
  };
}

/** Bounded snapshot of caller metadata, shared by rendering and path replacement. */
export function treeFieldMap(
  value: ReadonlyMap<string, Field>,
): ReadonlyMap<string, Field> {
  if (!(value instanceof Map) || value.size > 1024) {
    throw new ClientError(
      "validation",
      "Field metadata must be a map of at most 1024 fields.",
    );
  }
  const snapshot = new Map<string, Field>();
  let bytes = 0;
  for (const [name, field] of value) {
    editorText(name, "Field identity", 256);
    if (
      !field || typeof field !== "object" ||
      !/^[a-zA-Z_][a-zA-Z0-9_]*$/.test(name) || field.name !== name
    ) throw new ClientError("validation", "Invalid field metadata identity.");
    editorText(field.label, "Field label");
    if (
      ![
        "char",
        "text",
        "boolean",
        "integer",
        "float",
        "monetary",
        "date",
        "datetime",
        "selection",
        "many2one",
        "one2many",
        "many2many",
        "binary",
        "json",
      ].includes(field.kind)
    ) {
      throw new ClientError(
        "unsupported",
        `Unsupported tree field kind: ${
          String(field.kind)
        } (a later release advanced controls).`,
      );
    }
    if (
      typeof field.required !== "boolean" || typeof field.readonly !== "boolean"
    ) throw new ClientError("validation", "Invalid field metadata policy.");
    if (field.relation !== null) {
      editorText(field.relation, "Field relation identity", 256);
      if (!/^[a-z][a-z0-9_]*(?:\.[a-z][a-z0-9_]*)*$/.test(field.relation)) {
        throw new ClientError("validation", "Invalid field relation metadata.");
      }
    }
    bytes +=
      new TextEncoder().encode(name + field.label + (field.relation ?? ""))
        .length;
    if (field.selection !== null) {
      if (!Array.isArray(field.selection) || field.selection.length > 1024) {
        throw new ClientError(
          "validation",
          "Field selection metadata exceeds 1024 choices.",
        );
      }
      const seen = new Set<string>();
      for (const pair of field.selection) {
        if (!Array.isArray(pair) || pair.length !== 2) {
          throw new ClientError(
            "validation",
            "Invalid field selection metadata.",
          );
        }
        editorText(pair[0], "Selection identity");
        editorText(pair[1], "Selection label");
        if (seen.has(pair[0])) {
          throw new ClientError(
            "validation",
            "Duplicate field selection identity.",
          );
        }
        seen.add(pair[0]);
        bytes += new TextEncoder().encode(pair[0] + pair[1]).length;
      }
    }
    if (bytes > 262144) {
      throw new ClientError(
        "validation",
        "Field metadata text exceeds 262144 bytes.",
      );
    }
    if (field.kind === "selection" && field.selection === null) {
      throw new ClientError(
        "validation",
        "Selection condition field requires choices.",
      );
    }
    snapshot.set(name, {
      ...field,
      selection: field.selection?.map((pair: readonly [string, string]) =>
        [pair[0], pair[1]] as const
      ) ??
        null,
    });
  }
  return snapshot;
}
