/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import {
  AuthoringError,
  type CodecResult,
  type ConditionLiteral,
  type ConditionNode,
  resolveTreeLimits,
  type TreeLimits,
  validateConditionTree,
} from "./condition-tree.js";
import {
  AuthoringReader,
  AuthoringWriter,
  writeConditionLiteral,
} from "./condition-literals.js";
import { codecFailure } from "./domain-editor-codec.js";

// Hard keywords and Python's immutable debug constant cannot identify expression fields.
// Soft keywords remain valid identifiers in expression position.
const RESERVED_FIELDS = new Set([
  "False",
  "None",
  "True",
  "and",
  "as",
  "assert",
  "async",
  "await",
  "break",
  "class",
  "continue",
  "def",
  "del",
  "elif",
  "else",
  "except",
  "finally",
  "for",
  "from",
  "global",
  "if",
  "import",
  "in",
  "is",
  "lambda",
  "nonlocal",
  "not",
  "or",
  "pass",
  "raise",
  "return",
  "try",
  "while",
  "with",
  "yield",
  "__debug__",
]);
function admitExpressionField(path: string, offset = 0): void {
  if (RESERVED_FIELDS.has(path)) {
    throw new AuthoringError(
      `Expression field ${path} is a reserved identifier and cannot be authored.`,
      offset,
      false,
      "unsupported",
    );
  }
}

const OPERATORS = new Set([
  "==",
  "!=",
  "<",
  ">",
  "<=",
  ">=",
  "in",
  "not in",
  "is",
  "is not",
]);
function compatible(operator: string, value: ConditionLiteral): void {
  if (!OPERATORS.has(operator)) {
    throw new AuthoringError(
      `Expression operator ${operator} is outside the authoring grammar.`,
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
      "Expression membership requires a list or tuple literal.",
    );
  }
  if (
    (operator === "is" || operator === "is not") && value.kind !== "none" &&
    value.kind !== "boolean"
  ) {
    throw new AuthoringError(
      "Expression identity requires None or a boolean literal.",
      0,
      false,
      "unsupported",
    );
  }
}
/** Author field/literal expressions locally; never evaluate a field or its truthiness. */
export function parseExpressionSource(
  source: string,
  allowedFields: readonly string[],
  limits?: TreeLimits,
): CodecResult {
  const admission = resolveTreeLimits(limits);
  if (typeof source !== "string") {
    throw new AuthoringError("Authored source must be text.");
  }
  const fieldError = (): never => {
    throw new AuthoringError(
      "Expression fields must be an array of at most 1024 technical names.",
    );
  };
  if (
    !Array.isArray(allowedFields) ||
    Object.getPrototypeOf(allowedFields) !== Array.prototype ||
    allowedFields.length > 1024 ||
    Reflect.ownKeys(allowedFields).length !== allowedFields.length + 1
  ) fieldError();
  let fieldBytes = 0;
  for (let index = 0; index < allowedFields.length; index++) {
    const descriptor = Object.getOwnPropertyDescriptor(
      allowedFields,
      String(index),
    );
    if (
      !descriptor || !Object.hasOwn(descriptor, "value") ||
      typeof descriptor.value !== "string" || descriptor.value.length > 1024
    ) return fieldError();
    fieldBytes += new TextEncoder().encode(descriptor.value as string).length;
    if (fieldBytes > 65536) fieldError();
  }
  try {
    // Literal source nesting is bounded independently; final tree admission below
    // charges represented depth, including the literal's position in the tree.
    const reader = new AuthoringReader(source, { ...admission, depth: 32 });
    let parentheses = 0;
    function sourceDepth(depth: number): void {
      // A depth-32 represented tree can require up to 31 precedence parentheses
      // in addition to its recursive operands. This envelope bounds parser frames.
      if (depth > 64) {
        throw new AuthoringError(
          "Expression source exceeds 64 recursive levels.",
          reader.offset,
          true,
        );
      }
    }
    const fields = new Set(allowedFields);
    let nextId = 0;
    const id = () => {
      reader.charge();
      return `n${nextId++}`;
    };
    const unsupported = (): never =>
      reader.error(
        "Expression is outside the authoring grammar.",
        "unsupported",
      );
    function parseOr(depth: number): ConditionNode {
      sourceDepth(depth);
      const children = [parseAnd(depth)];
      while (reader.word("or")) children.push(parseAnd(depth));
      return children.length === 1
        ? children[0]!
        : { id: id(), kind: "or", children };
    }
    function parseAnd(depth: number): ConditionNode {
      sourceDepth(depth);
      const children = [parseNot(depth)];
      while (reader.word("and")) children.push(parseNot(depth));
      return children.length === 1
        ? children[0]!
        : { id: id(), kind: "and", children };
    }
    function parseNot(depth: number): ConditionNode {
      sourceDepth(depth);
      if (reader.word("not")) {
        return { id: id(), kind: "not", child: parseNot(depth + 1) };
      }
      return atom(depth);
    }
    function atom(depth: number): ConditionNode {
      sourceDepth(depth);
      if (reader.consume("(")) {
        if (++parentheses > 32) {
          throw new AuthoringError(
            "Expression source exceeds 32 parenthesis levels.",
            reader.offset,
            true,
          );
        }
        const node = parseOr(depth + 1);
        if (!reader.consume(")")) {
          reader.error("Expected closing expression parenthesis.");
        }
        parentheses--;
        return node;
      }
      if (reader.word("True")) {
        return { id: id(), kind: "constant", value: true };
      }
      if (reader.word("False")) {
        return { id: id(), kind: "constant", value: false };
      }
      reader.skip();
      const match = /^[a-zA-Z_][a-zA-Z0-9_]*(?:\.[a-zA-Z_][a-zA-Z0-9_]*)*/.exec(
        source.slice(reader.offset),
      );
      if (!match) {
        if (reader.peek() === "") {
          reader.error("Expected an expression operand.");
        }
        return unsupported();
      }
      const path = match[0];
      reader.offset += path.length;
      if (reader.peek() === "(") unsupported();
      if (path.includes(".")) {
        reader.error(
          "Expression authoring does not traverse relations.",
          "unsupported",
        );
      }
      admitExpressionField(path, reader.offset - path.length);
      if (!fields.has(path)) {
        reader.error(
          `Expression field ${path} is not declared for this editor.`,
          "unsupported",
        );
      }
      let operator: string | undefined;
      for (const candidate of ["==", "!=", "<=", ">=", "<", ">"]) {
        if (reader.consume(candidate)) {
          operator = candidate;
          break;
        }
      }
      if (!operator) {
        if (reader.word("in")) operator = "in";
        else if (reader.word("not")) {
          if (!reader.word("in")) unsupported();
          operator = "not in";
        } else if (reader.word("is")) {
          operator = reader.word("not") ? "is not" : "is";
        }
      }
      if (!operator) return { id: id(), kind: "field", path };
      let value: ConditionLiteral;
      try {
        value = reader.literal(1);
      } catch (error) {
        if (error instanceof AuthoringError && error.kind === "unsupported") {
          unsupported();
        }
        throw error;
      }
      compatible(operator, value);
      return { id: id(), kind: "condition", path, operator, value };
    }
    const tree = parseOr(1);
    if (reader.peek() !== "") unsupported();
    validateConditionTree(tree, admission);
    return { status: "supported", source, tree };
  } catch (error) {
    return codecFailure(source, error);
  }
}
/** Serialize with precedence parentheses so bare operands and NOT retain their semantics. */
export function serializeExpressionTree(
  tree: ConditionNode,
  limits?: TreeLimits,
): string {
  validateConditionTree(tree, limits);
  const writer = new AuthoringWriter(limits);
  function precedence(node: ConditionNode): number {
    return node.kind === "or"
      ? 1
      : node.kind === "and"
      ? 2
      : node.kind === "not"
      ? 3
      : 4;
  }
  function visit(node: ConditionNode, outer = 0): void {
    const own = precedence(node);
    if (own < outer) writer.push("(");
    switch (node.kind) {
      case "constant":
        writer.push(node.value ? "True" : "False");
        break;
      case "field":
        if (node.path.includes(".")) {
          throw new AuthoringError(
            "Expression authoring does not traverse relations.",
            0,
            false,
            "unsupported",
          );
        }
        admitExpressionField(node.path);
        writer.push(node.path);
        break;
      case "condition":
        if (node.path.includes(".")) {
          throw new AuthoringError(
            "Expression authoring does not traverse relations.",
            0,
            false,
            "unsupported",
          );
        }
        admitExpressionField(node.path);
        compatible(node.operator, node.value);
        writer.push(`${node.path} ${node.operator} `);
        writeConditionLiteral(node.value, writer);
        break;
      case "not":
        writer.push("not ");
        visit(node.child, own);
        break;
      case "and":
      case "or":
        if (node.children.length === 0) {
          writer.push(node.kind === "and" ? "True" : "False");
        }
        node.children.forEach((child, index) => {
          if (index > 0) writer.push(` ${node.kind} `);
          visit(child, own);
        });
        break;
    }
    if (own < outer) writer.push(")");
  }
  visit(tree);
  return writer.finish();
}
