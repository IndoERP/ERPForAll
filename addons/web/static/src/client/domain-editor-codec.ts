/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import {
  AuthoringError,
  type CodecResult,
  type ConditionLiteral,
  type ConditionNode,
  resolveTreeLimits,
  type TreeLimits,
  validateConditionPath,
  validateConditionTree,
} from "./condition-tree.js";
import {
  AuthoringReader,
  AuthoringWriter,
  writeConditionLiteral,
} from "./condition-literals.js";
import { ClientError } from "./transport.js";

const OPERATORS = new Set([
  "=",
  "!=",
  "=?",
  "<",
  ">",
  "<=",
  ">=",
  "in",
  "not in",
  "like",
  "not like",
  "ilike",
  "not ilike",
  "=like",
  "not =like",
  "=ilike",
  "not =ilike",
]);
/** Operator/value admission is independent of caller metadata compatibility. */
export function validateDomainCondition(
  operator: string,
  value: ConditionLiteral,
): void {
  if (operator === "any!" || operator === "not any!") {
    throw new AuthoringError(
      `Internal domain operator ${operator} is forbidden.`,
    );
  }
  if (!OPERATORS.has(operator)) {
    throw new AuthoringError(
      `Domain operator ${operator} has no authoring control in a later release.`,
      0,
      false,
      "unsupported",
    );
  }
  if (
    (operator === "in" || operator === "not in") && value.kind !== "list" &&
    value.kind !== "tuple"
  ) {
    throw new AuthoringError(
      "Domain membership requires a list or tuple literal.",
    );
  }
  if (operator.includes("like") && value.kind !== "string") {
    throw new AuthoringError("Domain text operators require a string literal.");
  }
}
function normalizeNone(value: ConditionLiteral): ConditionLiteral {
  if (value.kind === "none") return { kind: "boolean", value: false };
  if (value.kind === "list" || value.kind === "tuple") {
    return { ...value, items: value.items.map(normalizeNone) };
  }
  return value;
}
/** Preserve exact source for all refusals; only the supported branch contains a tree. */
export function codecFailure(source: string, error: unknown): CodecResult {
  if (!(error instanceof ClientError)) throw error;
  const kind = error.kind === "unsupported" ? "unsupported" : "validation";
  return {
    status: kind === "unsupported" ? "unsupported" : "invalid",
    source,
    error: {
      kind,
      message: error.message,
      offset: error instanceof AuthoringError ? error.offset : 0,
      ...(error instanceof AuthoringError && error.admission
        ? { admission: true as const }
        : {}),
    },
  };
}
/** Parse bounded domain authoring, flattening only associative AND/OR groups. */
export function parseDomainSource(
  source: string,
  limits?: TreeLimits,
): CodecResult {
  const admission = resolveTreeLimits(limits);
  if (typeof source !== "string") {
    throw new AuthoringError("Authored source must be text.");
  }
  try {
    const reader = new AuthoringReader(source, limits, true);
    const value = reader.literal();
    reader.end("Unexpected text after the domain.");
    let nextId = 0;
    const id = () => {
      if (nextId >= 5 * admission.nodes + 1) {
        throw new AuthoringError(
          `Condition tree exceeds ${admission.nodes} nodes.`,
          0,
          true,
        );
      }
      return `n${nextId++}`;
    };
    function group(
      kind: "and" | "or",
      items: readonly ConditionNode[],
    ): ConditionNode {
      const children: ConditionNode[] = [];
      for (const item of items) {
        if (item.kind === kind) children.push(...item.children);
        else children.push(item);
      }
      return { id: id(), kind, children };
    }
    if (value.kind === "boolean") {
      return {
        status: "supported",
        source,
        tree: { id: id(), kind: "constant", value: value.value },
      };
    }
    if (value.kind !== "list") {
      throw new AuthoringError("Domain source must be a list or boolean.");
    }
    const stack: ConditionNode[] = [];
    for (let index = value.items.length - 1; index >= 0; index--) {
      const item = value.items[index]!;
      if (item.kind === "string" && ["&", "|", "!"].includes(item.value)) {
        const child = stack.pop();
        if (!child) {
          throw new AuthoringError(
            `Domain connector ${item.value} requires ${
              item.value === "!" ? "one operand" : "two operands"
            }.`,
          );
        }
        if (item.value === "!") {
          stack.push({ id: id(), kind: "not", child });
          continue;
        }
        const right = stack.pop();
        if (!right) {
          throw new AuthoringError(
            `Domain connector ${item.value} requires two operands.`,
          );
        }
        stack.push(group(item.value === "&" ? "and" : "or", [child, right]));
        continue;
      }
      if (
        (item.kind !== "tuple" && item.kind !== "list") ||
        item.items.length !== 3
      ) {
        throw new AuthoringError(
          "Domain conditions must contain exactly three items.",
        );
      }
      const [field, operator, operand] = item.items;
      if (!field || !operator || !operand || operator.kind !== "string") {
        throw new AuthoringError("Domain condition operator must be a string.");
      }
      if (
        field.kind === "integer" && operator.value === "=" &&
        operand.kind === "integer" && operand.value === "1" &&
        (field.value === "0" || field.value === "1")
      ) {
        stack.push({ id: id(), kind: "constant", value: field.value === "1" });
        continue;
      }
      if (field.kind !== "string") {
        throw new AuthoringError(
          "Domain condition field must be a technical path.",
        );
      }
      validateConditionPath(field.value);
      const normalized = normalizeNone(operand);
      validateDomainCondition(operator.value, normalized);
      stack.push({
        id: id(),
        kind: "condition",
        path: field.value,
        operator: operator.value,
        value: normalized,
      });
    }
    const children = stack.reverse();
    const tree: ConditionNode = children.length === 1
      ? children[0]!
      : group("and", children);
    validateConditionTree(tree, limits);
    return { status: "supported", source, tree };
  } catch (error) {
    return codecFailure(source, error);
  }
}
/** Serialize only admitted domain trees; an unsupported node never becomes an empty domain. */
export function serializeDomainTree(
  tree: ConditionNode,
  limits?: TreeLimits,
): string {
  validateConditionTree(tree, limits);
  const writer = new AuthoringWriter(limits);
  writer.push("[");
  let first = true;
  function token(write: () => void): void {
    if (!first) writer.push(", ");
    first = false;
    write();
  }
  function text(value: string): void {
    token(() => writeConditionLiteral({ kind: "string", value }, writer));
  }
  function visit(node: ConditionNode): void {
    switch (node.kind) {
      case "field":
        throw new AuthoringError(
          "Bare field operands cannot be authored as domains.",
        );
      case "constant":
        token(() => writer.push(node.value ? "(1, '=', 1)" : "(0, '=', 1)"));
        break;
      case "condition":
        validateDomainCondition(node.operator, node.value);
        token(() => {
          writer.push("(");
          writeConditionLiteral({ kind: "string", value: node.path }, writer);
          writer.push(", ");
          writeConditionLiteral(
            { kind: "string", value: node.operator },
            writer,
          );
          writer.push(", ");
          writeConditionLiteral(normalizeNone(node.value), writer);
          writer.push(")");
        });
        break;
      case "not":
        text("!");
        visit(node.child);
        break;
      case "and":
      case "or":
        if (node.children.length === 0) {
          if (node.kind === "or") token(() => writer.push("(0, '=', 1)"));
          else if (node !== tree) token(() => writer.push("(1, '=', 1)"));
          break;
        }
        for (let index = 1; index < node.children.length; index++) {
          text(node.kind === "and" ? "&" : "|");
        }
        node.children.forEach(visit);
        break;
    }
  }
  visit(tree);
  writer.push("]");
  return writer.finish();
}
