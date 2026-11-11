/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { ClientError } from "./transport.js";
/** Authoring literals retain integer and float identity; this is not an execution AST. */
export type ConditionLiteral =
  | { readonly kind: "string"; readonly value: string }
  | { readonly kind: "boolean"; readonly value: boolean }
  | { readonly kind: "none" }
  | { readonly kind: "integer"; readonly value: string }
  | { readonly kind: "float"; readonly value: number }
  | {
    readonly kind: "list" | "tuple";
    readonly items: readonly ConditionLiteral[];
  };
/** Immutable, editor-local authoring tree. */
export type ConditionNode =
  | { readonly id: string; readonly kind: "constant"; readonly value: boolean }
  | { readonly id: string; readonly kind: "field"; readonly path: string }
  | {
    readonly id: string;
    readonly kind: "condition";
    readonly path: string;
    readonly operator: string;
    readonly value: ConditionLiteral;
  }
  | {
    readonly id: string;
    readonly kind: "and" | "or";
    readonly children: readonly ConditionNode[];
  }
  | {
    readonly id: string;
    readonly kind: "not";
    readonly child: ConditionNode;
  };
/** Positive lowerings's browser admission ceilings. */
export interface TreeLimits {
  readonly sourceBytes?: number;
  readonly outputBytes?: number;
  readonly nodes?: number;
  readonly depth?: number;
}
/** Source admission result, never an executable AST or server authorization. */
export type CodecResult =
  | {
    readonly status: "supported";
    readonly source: string;
    readonly tree: ConditionNode;
  }
  | {
    readonly status: "unsupported" | "invalid";
    readonly source: string;
    readonly error: {
      readonly kind: "validation" | "unsupported";
      readonly message: string;
      readonly offset: number;
      readonly admission?: true;
    };
  };
/** Hard safety ceilings; callers may only lower them. */
export const AUTHORING_LIMITS: Required<TreeLimits> = Object.freeze({
  sourceBytes: 65536,
  outputBytes: 65536,
  nodes: 1024,
  depth: 32,
});
/** Validate all limit keys and positive lowerings before parsing or allocating a tree. */
export function resolveTreeLimits(
  limits: TreeLimits = {},
): Required<TreeLimits> {
  plain(
    limits,
    ["sourceBytes", "outputBytes", "nodes", "depth"],
    "Editor limits",
  );
  const result = { ...AUTHORING_LIMITS };
  for (const key of ["sourceBytes", "outputBytes", "nodes", "depth"] as const) {
    const value = Object.hasOwn(limits, key) ? limits[key] : undefined;
    if (value !== undefined) {
      if (
        typeof value !== "number" || !Number.isSafeInteger(value) ||
        value < 1 ||
        value > AUTHORING_LIMITS[key]
      ) {
        fail(
          `Editor ${key} limit must be an integer from 1 to ${
            AUTHORING_LIMITS[key]
          }.`,
        );
      }
      result[key] = value;
    }
  }
  return result;
}
/** Internal parser/refusal error retaining textarea offset and admission classification. */
export class AuthoringError extends ClientError {
  constructor(
    message: string,
    readonly offset = 0,
    readonly admission = false,
    kind: "validation" | "unsupported" = "validation",
  ) {
    super(kind, message);
  }
}
export function fail(message: string): never {
  throw new AuthoringError(message);
}
export function failAdmission(message: string): never {
  throw new AuthoringError(message, 0, true);
}
/** Source and output checks are separate so raw editors can refuse admission before callbacks. */
export function checkAuthoredSource(source: string, limits?: TreeLimits): void {
  textBytes(source, resolveTreeLimits(limits).sourceBytes, "Authored source");
}
export function checkAuthoredOutput(source: string, limits?: TreeLimits): void {
  textBytes(source, resolveTreeLimits(limits).outputBytes, "Authored output");
}
function textBytes(value: unknown, maximum: number, label: string): number {
  if (typeof value !== "string") fail(`${label} must be text.`);
  if (value.length > maximum) {
    failAdmission(`${label} exceeds ${maximum} UTF-8 bytes.`);
  }
  const bytes = new TextEncoder().encode(value).length;
  if (bytes > maximum) {
    failAdmission(`${label} exceeds ${maximum} UTF-8 bytes.`);
  }
  return bytes;
}
function plain(
  value: unknown,
  keys: readonly string[],
  label: string,
): asserts value is Record<string, unknown> {
  if (
    !value || typeof value !== "object" ||
    ![Object.prototype, null].includes(Object.getPrototypeOf(value))
  ) fail(`${label} must be a plain object.`);
  for (const key of Reflect.ownKeys(value)) {
    if (typeof key !== "string" || !keys.includes(key)) {
      fail(`Unknown ${label.toLowerCase()} key: ${String(key)}.`);
    }
    if (
      !Object.getOwnPropertyDescriptor(value, key)?.enumerable ||
      !Object.hasOwn(Object.getOwnPropertyDescriptor(value, key)!, "value")
    ) fail(`${label} must contain data properties.`);
  }
}
/** Validate a technical field path using the existing selector's admission bounds. */
export function validateConditionPath(path: unknown): asserts path is string {
  textBytes(path, 1024, "Condition field path");
  if (
    typeof path !== "string" || path.split(".").length > 32 ||
    !/^[a-zA-Z_][a-zA-Z0-9_]*(?:\.[a-zA-Z_][a-zA-Z0-9_]*)*$/.test(path)
  ) fail("Condition field path must contain valid technical names.");
}
interface Budget {
  count: number;
  bytes: number;
  readonly active: Set<object>;
  readonly ids: Set<string>;
  readonly limits: Required<TreeLimits>;
}
function enter(
  value: unknown,
  depth: number,
  budget: Budget,
): asserts value is Record<string, unknown> {
  if (!value || typeof value !== "object") {
    fail("Condition tree entries must be objects.");
  }
  if (budget.active.has(value)) fail("Condition tree contains a cycle.");
  plain(value, [
    "id",
    "kind",
    "value",
    "path",
    "operator",
    "children",
    "child",
    "items",
  ], "Condition tree entry");
  if (depth > budget.limits.depth) {
    failAdmission(`Condition tree exceeds ${budget.limits.depth} levels.`);
  }
  if (++budget.count > budget.limits.nodes) {
    failAdmission(`Condition tree exceeds ${budget.limits.nodes} nodes.`);
  }
  budget.active.add(value);
}
function charge(
  value: unknown,
  label: string,
  budget: Budget,
  maximum = 65536,
): void {
  budget.bytes += textBytes(value, maximum, label);
  if (budget.bytes > 65536) {
    failAdmission("Condition tree text exceeds 65536 UTF-8 bytes.");
  }
}
function literal(value: unknown, depth: number, budget: Budget): void {
  enter(value, depth, budget);
  switch (value["kind"]) {
    case "none":
      plain(value, ["kind"], "Condition literal");
      break;
    case "string":
      plain(value, ["kind", "value"], "Condition literal");
      charge(value["value"], "Condition string", budget);
      break;
    case "integer":
      plain(value, ["kind", "value"], "Condition literal");
      if (
        typeof value["value"] !== "string" || value["value"].length > 20 ||
        !/^(?:0|-[1-9][0-9]*|[1-9][0-9]*)$/.test(value["value"])
      ) fail("Integer literal must be canonical signed 64-bit decimal text.");
      if (
        BigInt(value["value"]) < -9223372036854775808n ||
        BigInt(value["value"]) > 9223372036854775807n
      ) fail("Integer is outside the signed 64-bit range.");
      break;
    case "float":
      plain(value, ["kind", "value"], "Condition literal");
      if (
        typeof value["value"] !== "number" || !Number.isFinite(value["value"])
      ) {
        fail("Float must be finite.");
      }
      break;
    case "boolean":
      plain(value, ["kind", "value"], "Condition literal");
      if (typeof value["value"] !== "boolean") {
        fail("Boolean literal must contain a boolean.");
      }
      break;
    case "list":
    case "tuple":
      plain(value, ["kind", "items"], "Condition literal");
      if (!Array.isArray(value["items"])) {
        fail("Collection literal items must be an array.");
      }
      if (value["items"].length > budget.limits.nodes - budget.count) {
        failAdmission(`Condition tree exceeds ${budget.limits.nodes} nodes.`);
      }
      dense(value["items"]);
      for (const item of value["items"]) literal(item, depth + 1, budget);
      break;
    default:
      fail("Unknown condition literal kind.");
  }
  budget.active.delete(value);
}
function dense(value: readonly unknown[]): void {
  if (
    Object.getPrototypeOf(value) !== Array.prototype ||
    Reflect.ownKeys(value).length !== value.length + 1
  ) fail("Condition collections must be dense data arrays.");
  for (let index = 0; index < value.length; index++) {
    const descriptor = Object.getOwnPropertyDescriptor(value, String(index));
    if (
      !descriptor || !Object.hasOwn(descriptor, "value") ||
      !descriptor.enumerable
    ) fail("Condition collections must be dense data arrays.");
  }
}
function makeBudget(limits?: TreeLimits): Budget {
  return {
    count: 0,
    bytes: 0,
    active: new Set(),
    ids: new Set(),
    limits: resolveTreeLimits(limits),
  };
}
/** Shared literal admission includes every nested item and refuses cycles. */
export function validateConditionLiteral(
  value: unknown,
  limits?: TreeLimits,
): asserts value is ConditionLiteral {
  literal(value, 1, makeBudget(limits));
}
/** Refuse malformed caller trees before cloning, rendering or serializing them. */
export function validateConditionTree(
  tree: unknown,
  limits?: TreeLimits,
): asserts tree is ConditionNode {
  const budget = makeBudget(limits);
  function visit(value: unknown, depth: number): void {
    enter(value, depth, budget);
    charge(value["id"], "Condition node ID", budget, 256);
    if (typeof value["id"] !== "string" || value["id"].length === 0) {
      fail("Condition node ID must not be empty.");
    }
    if (budget.ids.has(value["id"])) {
      fail(`Condition node IDs must be unique: ${value["id"]}.`);
    }
    budget.ids.add(value["id"]);
    switch (value["kind"]) {
      case "constant":
        plain(value, ["id", "kind", "value"], "Condition node");
        if (typeof value["value"] !== "boolean") {
          fail("Condition constant must contain a boolean.");
        }
        break;
      case "field":
        plain(value, ["id", "kind", "path"], "Condition node");
        validateConditionPath(value["path"]);
        charge(value["path"], "Condition field path", budget, 1024);
        break;
      case "condition":
        plain(
          value,
          ["id", "kind", "path", "operator", "value"],
          "Condition node",
        );
        validateConditionPath(value["path"]);
        charge(value["path"], "Condition field path", budget, 1024);
        charge(value["operator"], "Condition operator", budget, 32);
        if (value["operator"] === "") {
          fail("Condition operator must not be empty.");
        }
        literal(value["value"], depth + 1, budget);
        break;
      case "not":
        plain(value, ["id", "kind", "child"], "Condition node");
        visit(value["child"], depth + 1);
        break;
      case "and":
      case "or":
        plain(value, ["id", "kind", "children"], "Condition node");
        if (!Array.isArray(value["children"])) {
          fail("Condition group children must be an array.");
        }
        if (value["children"].length > budget.limits.nodes - budget.count) {
          failAdmission(`Condition tree exceeds ${budget.limits.nodes} nodes.`);
        }
        dense(value["children"]);
        for (const child of value["children"]) visit(child, depth + 1);
        break;
      default:
        fail("Unknown condition node kind.");
    }
    budget.active.delete(value);
  }
  visit(tree, 1);
}
function transform(
  tree: ConditionNode,
  id: string,
  update: (
    node: ConditionNode,
    parent?: ConditionNode,
  ) => ConditionNode | undefined,
  limits?: TreeLimits,
): ConditionNode {
  validateConditionTree(tree, limits);
  let found = false;
  function walk(
    node: ConditionNode,
    parent?: ConditionNode,
  ): ConditionNode | undefined {
    if (node.id === id) {
      found = true;
      return update(node, parent);
    }
    if (node.kind === "and" || node.kind === "or") {
      const children: ConditionNode[] = [];
      for (const child of node.children) {
        const changed = walk(child, node);
        if (changed !== undefined) children.push(changed);
      }
      return children.length === 0 && node.children.length !== 0
        ? undefined
        : { ...node, children };
    }
    if (node.kind === "not") {
      const child = walk(node.child, node);
      return child === undefined ? undefined : { ...node, child };
    }
    return node;
  }
  const edited = walk(tree);
  if (!found) fail(`Condition node ${id} does not exist.`);
  const result: ConditionNode = edited ??
    { id: tree.id, kind: "and", children: [] };
  validateConditionTree(result, limits);
  return result;
}
/** Replace one node atomically; failures leave the original untouched. */
export function replaceConditionNode(
  tree: ConditionNode,
  id: string,
  replacement: ConditionNode,
  limits?: TreeLimits,
): ConditionNode {
  validateConditionTree(replacement, limits);
  return transform(tree, id, () => replacement, limits);
}
/** Delete and prune empty ancestors, retaining an empty root editing container. */
export function removeConditionNode(
  tree: ConditionNode,
  id: string,
  limits?: TreeLimits,
): ConditionNode {
  return transform(tree, id, () => undefined, limits);
}
/** Append only to an admitted AND/OR group. */
export function appendConditionNode(
  tree: ConditionNode,
  parentId: string,
  node: ConditionNode,
  limits?: TreeLimits,
): ConditionNode {
  validateConditionTree(node, limits);
  return transform(tree, parentId, (parent) => {
    if (parent.kind !== "and" && parent.kind !== "or") {
      fail("Conditions can only be appended to AND/OR groups.");
    }
    return { ...parent, children: [...parent.children, node] };
  }, limits);
}
/** Wrap a node in an alternating group while preserving its identity. */
export function nestConditionNode(
  tree: ConditionNode,
  id: string,
  newId: string,
  limits?: TreeLimits,
): ConditionNode {
  return transform(
    tree,
    id,
    (node, parent) => ({
      id: newId,
      kind: parent?.kind === "and" ? "or" : "and",
      children: [node],
    }),
    limits,
  );
}
