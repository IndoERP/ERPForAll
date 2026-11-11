/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { WireDocument } from "../../../src/generated/WireDocument";
import type { WireNode } from "../../../src/generated/WireNode";
import { fail, fieldPath, lowerLimits, type AllocationCounter } from "./errors";
import { parsedMetadata, parsedPath, type JsonValue } from "./reader";

export const AST_LIMITS = Object.freeze({ maxExpressionBytes: 16 * 1024 * 1024, maxNodes: 65536 as number, maxJsonDepth: 16 as number, maxHeight: 100 as number, maxDescriptionBytes: 32 * 1024 * 1024, maxExpressions: 1024 as number, maxTotalNodes: 262144 as number });
export type AstLimits = Partial<typeof AST_LIMITS>;
export type Immutable<T> = T extends readonly (infer Item)[] ? readonly Immutable<Item>[] : T extends object ? { readonly [K in keyof T]: Immutable<T[K]> } : T;
/** Consumers must call isValidatedAst at runtime when accepting a handle across a trust boundary. */
export interface ValidatedAst { readonly nodes: readonly Immutable<WireNode>[]; readonly height: number; }
const admitted = new WeakSet<object>();
const admittedSites = new WeakMap<object, number>();
/** Successful decoder application sites only; inspection itself is excluded. */
export function inspectValidated(value: unknown): Readonly<{ applicationAllocations: number }> {
  if (!isValidatedAst(value)) fail("$", "expected authenticated validated AST");
  return Object.freeze({ applicationAllocations: admittedSites.get(value)! });
}
function childPath(counter: AllocationCounter, prefix: string, field: string): string {
  counter.sites++; return `${prefix}.${field}`;
}
function indexedPath(counter: AllocationCounter, prefix: string, field: string, index: number): string {
  counter.sites++; return `${prefix}.${field}[${index}]`;
}
function entries<T>(counter: AllocationCounter, value: readonly T[]): ArrayIterator<[number, T]> {
  counter.sites++; return value.entries();
}
interface BudgetState { readonly limits: typeof AST_LIMITS; readonly scope: ReturnType<typeof parsedMetadata>; context: ReturnType<typeof parsedMetadata>; expressions: number; nodes: number; }
const budgets = new WeakMap<object, BudgetState>();

/** Shared admission budget for all expressions in one parsed description.
 * Without an explicit subtree, the entire original response counts toward description bytes.
 */
export class DescriptionBudget {
  constructor(limits: AstLimits = {}, description?: unknown) {
    const scope = description === undefined ? undefined : parsedMetadata(description);
    if (description !== undefined && !scope) fail("$", "description must originate from the bounded JSON reader");
    const resolved = lowerLimits(AST_LIMITS, limits);
    if (scope && scope.end - scope.start > resolved.maxDescriptionBytes) fail(parsedPath(scope), `description byte limit exceeded (${resolved.maxDescriptionBytes})`);
    budgets.set(this, { limits: resolved, scope, context: scope, expressions: 0, nodes: 0 });
    Object.freeze(this);
  }
}

export function isValidatedAst(value: unknown): value is ValidatedAst {
  return typeof value === "object" && value !== null && admitted.has(value);
}
function record(value: JsonValue | undefined, path: string): Record<string, JsonValue> {
  if (value === null || typeof value !== "object" || Array.isArray(value)) fail(path, "expected object");
  return value as Record<string, JsonValue>;
}
function fields(counter: AllocationCounter, value: Record<string, JsonValue>, path: string, expected: readonly string[]): void {
  // One field-list literal at every call site and the Object.keys result below.
  counter.sites += 2;
  for (const key of Object.keys(value)) if (!expected.includes(key)) fail(fieldPath(path, key), "unknown field");
  for (const key of expected) if (!Object.hasOwn(value, key)) fail(fieldPath(path, key), "missing required field");
}
function array(value: JsonValue | undefined, path: string): readonly JsonValue[] {
  if (!Array.isArray(value)) fail(path, "expected array");
  return value;
}
function string(value: JsonValue | undefined, path: string): string {
  if (typeof value !== "string") fail(path, "expected string");
  return value;
}
function optionalString(value: JsonValue | undefined, path: string): void { if (value !== null) string(value, path); }
function member(value: JsonValue | undefined, allowed: readonly string[], path: string): void {
  if (typeof value !== "string" || !allowed.includes(value)) fail(path, "unknown operator");
}
const UNARY = ["u_add", "u_sub", "not", "invert"];
const BINARY = ["add", "sub", "mult", "div", "floor_div", "mod", "pow", "bit_and", "bit_or", "bit_xor", "l_shift", "r_shift"];
const COMPARE = ["eq", "not_eq", "lt", "lt_e", "gt", "gt_e", "in", "not_in", "is", "is_not"];
function literal(counter: AllocationCounter, raw: JsonValue | undefined, path: string): void {
  const value = record(raw, path);
  counter.sites++; // Literal-kind allowlist array at this call site.
  if (!["none", "bool", "int", "float", "str", "bytes"].includes(value["kind"] as string)) fail(childPath(counter, path, "kind"), "unknown literal kind");
  fields(counter, value, path, value["kind"] === "none" ? ["kind"] : ["kind", "value"]);
  const payload = value["value"];
  const payloadPath = childPath(counter, path, "value");
  switch (value["kind"]) {
    case "none": return;
    case "bool": if (typeof payload !== "boolean") fail(payloadPath, "expected boolean"); return;
    case "int": {
      counter.sites++; // Canonical decimal regexp literal.
      // Check length before BigInt construction: hostile decimal strings are bounded work.
      if (typeof payload !== "string" || payload.length > 20 || !/^(?:0|-?[1-9][0-9]*)$/.test(payload)) fail(payloadPath, "expected canonical i64 decimal string");
      counter.sites++; // BigInt value construction.
      const integer = BigInt(payload);
      if (integer < -9223372036854775808n || integer > 9223372036854775807n) fail(payloadPath, "expected canonical i64 decimal string");
      return;
    }
    case "float": counter.sites++; if (typeof payload !== "string" || !/^[0-9a-f]{16}$/.test(payload)) fail(payloadPath, "expected 16 lowercase hexadecimal float bits"); return;
    case "str": string(payload, payloadPath); return;
    case "bytes":
      for (const [index, byte] of entries(counter, array(payload, payloadPath))) {
        if (typeof byte !== "number" || !Number.isInteger(byte) || byte < 0 || byte > 255) fail(`${payloadPath}[${index}]`, "expected byte integer in 0..255");
      }
      return;
    default: fail(childPath(counter, path, "kind"), "unknown literal kind");
  }
}

/** Internal structural baseline for tests/measurement only. Inputs must be inert JSON values.
 * Not exported by the package and never used to authenticate public handles.
 */
export function validateStructure(raw: JsonValue, limits: typeof AST_LIMITS = AST_LIMITS, path = "$", counter: AllocationCounter = { sites: 1 }): { document: WireDocument; height: number; applicationAllocations: number } {
  const document = record(raw, path);
  fields(counter, document, path, ["schema", "nodes"]);
  if (typeof document["schema"] !== "number" || !Number.isSafeInteger(document["schema"])) fail(childPath(counter, path, "schema"), "expected schema version integer");
  if (document["schema"] !== 1) fail(childPath(counter, path, "schema"), `unsupported schema version ${document["schema"]}; reload or upgrade required`);
  const nodes = array(document["nodes"], childPath(counter, path, "nodes"));
  if (nodes.length === 0 || nodes.length > limits.maxNodes) fail(childPath(counter, path, "nodes"), `expected 1..=${limits.maxNodes} nodes`);
  counter.sites += 2; // The two arena-validation arrays.
  const parents = new Uint8Array(nodes.length);
  const heights = new Uint16Array(nodes.length);
  for (const [index, rawNode] of entries(counter, nodes)) {
    const nodePath = indexedPath(counter, path, "nodes", index);
    const node = record(rawNode, nodePath);
    let maximum = 0;
    let clauses = 0;
    counter.sites += 2; // Per-node edge and edges closures.
    function edge(value: JsonValue | undefined): void {
      if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0 || value >= index) fail(nodePath, `child index ${typeof value === "number" ? value : "<invalid>"} must point strictly backwards`);
      if (parents[value] !== 0) fail(nodePath, `child index ${value} has more than one incoming edge`);
      parents[value] = 1;
      maximum = Math.max(maximum, heights[value]!);
    }
    function edges(value: JsonValue | undefined, field: string): readonly JsonValue[] {
      const children = array(value, field);
      for (const child of children) edge(child);
      return children;
    }
    switch (node["kind"]) {
      case "literal": fields(counter, node, nodePath, ["kind", "value"]); literal(counter, node["value"], childPath(counter, nodePath, "value")); break;
      case "name": fields(counter, node, nodePath, ["kind", "name"]); string(node["name"], childPath(counter, nodePath, "name")); break;
      case "unary": fields(counter, node, nodePath, ["kind", "op", "operand"]); member(node["op"], UNARY, childPath(counter, nodePath, "op")); edge(node["operand"]); break;
      case "binary": fields(counter, node, nodePath, ["kind", "op", "left", "right"]); member(node["op"], BINARY, childPath(counter, nodePath, "op")); edge(node["left"]); edge(node["right"]); break;
      case "bool_op": {
        counter.sites++; // Per-node boolean-operator allowlist literal.
        fields(counter, node, nodePath, ["kind", "op", "values"]); member(node["op"], ["and", "or"], childPath(counter, nodePath, "op"));
        const values = array(node["values"], childPath(counter, nodePath, "values"));
        if (values.length < 2) fail(childPath(counter, nodePath, "values"), "boolean operation requires at least two values");
        edges(values, childPath(counter, nodePath, "values")); break;
      }
      case "compare": {
        fields(counter, node, nodePath, ["kind", "left", "ops", "comparators"]);
        const ops = array(node["ops"], childPath(counter, nodePath, "ops"));
        const comparators = array(node["comparators"], childPath(counter, nodePath, "comparators"));
        if (ops.length === 0 || ops.length !== comparators.length) fail(childPath(counter, nodePath, "comparators"), "comparison requires matching nonempty operators and comparators");
        for (const [opIndex, op] of entries(counter, ops)) member(op, COMPARE, indexedPath(counter, nodePath, "ops", opIndex));
        edge(node["left"]); edges(comparators, childPath(counter, nodePath, "comparators")); break;
      }
      case "if_exp": fields(counter, node, nodePath, ["kind", "test", "body", "orelse"]); edge(node["test"]); edge(node["body"]); edge(node["orelse"]); break;
      case "attribute": fields(counter, node, nodePath, ["kind", "value", "attr"]); string(node["attr"], childPath(counter, nodePath, "attr")); edge(node["value"]); break;
      case "subscript": {
        fields(counter, node, nodePath, ["kind", "value", "slice"]);
        const slicePath = childPath(counter, nodePath, "slice");
        const slice = record(node["slice"], slicePath);
        if (slice["kind"] === "index") { fields(counter, slice, slicePath, ["kind", "value"]); edge(slice["value"]); }
        else if (slice["kind"] === "slice") {
          fields(counter, slice, slicePath, ["kind", "lower", "upper", "step"]);
          counter.sites++; // Slice-field traversal array literal.
          for (const key of ["lower", "upper", "step"]) if (slice[key] !== null) edge(slice[key]);
        } else fail(childPath(counter, slicePath, "kind"), "unknown subscript kind");
        edge(node["value"]); break;
      }
      case "call": {
        fields(counter, node, nodePath, ["kind", "func", "args", "keywords"]);
        edge(node["func"]); edges(node["args"], childPath(counter, nodePath, "args"));
        for (const [keywordIndex, rawKeyword] of entries(counter, array(node["keywords"], childPath(counter, nodePath, "keywords")))) {
          const keywordPath = indexedPath(counter, nodePath, "keywords", keywordIndex);
          const keyword = record(rawKeyword, keywordPath);
          fields(counter, keyword, keywordPath, ["name", "value"]);
          optionalString(keyword["name"], childPath(counter, keywordPath, "name")); edge(keyword["value"]);
        }
        break;
      }
      case "list": case "tuple": case "joined_str": fields(counter, node, nodePath, ["kind", "items"]); edges(node["items"], childPath(counter, nodePath, "items")); break;
      case "dict": {
        fields(counter, node, nodePath, ["kind", "items"]);
        for (const [itemIndex, rawItem] of entries(counter, array(node["items"], childPath(counter, nodePath, "items")))) {
          const itemPath = indexedPath(counter, nodePath, "items", itemIndex);
          const item = array(rawItem, itemPath);
          if (item.length !== 2) fail(itemPath, "expected key/value pair");
          edge(item[0]); edge(item[1]);
        }
        break;
      }
      case "walrus": fields(counter, node, nodePath, ["kind", "name", "value"]); string(node["name"], childPath(counter, nodePath, "name")); edge(node["value"]); break;
      case "formatted_value": {
        fields(counter, node, nodePath, ["kind", "value", "conversion", "format_spec"]);
        const conversion = node["conversion"];
        if (conversion !== null && (typeof conversion !== "string" || (conversion.length !== 1 && !(conversion.length === 2 && conversion.codePointAt(0)! > 0xffff)))) fail(childPath(counter, nodePath, "conversion"), "expected one Unicode scalar or null");
        optionalString(node["format_spec"], childPath(counter, nodePath, "format_spec")); edge(node["value"]); break;
      }
      case "list_comp": case "generator_exp": {
        fields(counter, node, nodePath, ["kind", "elt", "generators"]);
        const generators = array(node["generators"], childPath(counter, nodePath, "generators"));
        if (generators.length === 0) fail(childPath(counter, nodePath, "generators"), "comprehension requires at least one clause");
        clauses = generators.length;
        edge(node["elt"]);
        for (const [generatorIndex, rawGenerator] of entries(counter, generators)) {
          const generatorPath = indexedPath(counter, nodePath, "generators", generatorIndex);
          const generator = record(rawGenerator, generatorPath);
          fields(counter, generator, generatorPath, ["target", "iter", "ifs"]);
          edge(generator["target"]); edge(generator["iter"]); edges(generator["ifs"], childPath(counter, generatorPath, "ifs"));
        }
        break;
      }
      case "starred": fields(counter, node, nodePath, ["kind", "value"]); edge(node["value"]); break;
      default: fail(childPath(counter, nodePath, "kind"), "unknown node kind");
    }
    const height = 1 + clauses + maximum;
    if (height > limits.maxHeight) fail(childPath(counter, path, "nodes"), `logical AST depth exceeds ${limits.maxHeight}`);
    heights[index] = height;
  }
  for (let index = 0; index + 1 < nodes.length; index++) if (parents[index] !== 1) fail(indexedPath(counter, path, "nodes", index), "nonroot node must have exactly one incoming edge");
  counter.sites++; // Validation result object.
  return { document: raw as unknown as WireDocument, height: heights[nodes.length - 1]!, applicationAllocations: counter.sites };
}

/** Admit only a frozen subtree authenticated by parseBoundedJson; no getters/proxies are visited. */
export function decodeAst(input: unknown, budget?: DescriptionBudget): ValidatedAst {
  const counter: AllocationCounter = { sites: 1 };
  if (budget === undefined) { budget = new DescriptionBudget(); counter.sites += 5; }
  const meta = parsedMetadata(input);
  if (!meta) fail("$", "AST must originate from the bounded JSON reader");
  const path = parsedPath(meta, counter);
  const state = budgets.get(budget);
  if (!state) fail(path, "invalid description budget");
  const { limits, scope } = state;
  const bytes = meta.end - meta.start;
  if (state.context && state.context.context !== meta.context) fail(path, "AST is outside its description");
  if (scope && (scope.context !== meta.context || meta.start < scope.start || meta.end > scope.end)) fail(path, "AST is outside its description");
  const descriptionBytes = scope ? scope.end - scope.start : meta.context.stats.bytes;
  if (descriptionBytes > limits.maxDescriptionBytes) fail(path, `description byte limit exceeded (${limits.maxDescriptionBytes})`);
  if (bytes > limits.maxExpressionBytes) fail(path, `encoded JSON exceeds ${limits.maxExpressionBytes} bytes`);
  if (meta.depth > limits.maxJsonDepth) fail(path, `JSON nesting exceeds ${limits.maxJsonDepth}`);
  if (state.expressions >= limits.maxExpressions) fail(path, `description expression limit exceeded (${limits.maxExpressions})`);
  const raw = record(input as JsonValue, path);
  const nodes = raw["nodes"];
  if (Array.isArray(nodes) && nodes.length > limits.maxTotalNodes - state.nodes) fail(path, `description node limit exceeded (${limits.maxTotalNodes})`);
  const result = validateStructure(input as JsonValue, limits, path, counter);
  state.context = meta;
  state.expressions++; state.nodes += result.document.nodes.length;
  // Parsed containers are already recursively frozen; retain them without a second parse/copy.
  counter.sites++; // Authenticated handle object.
  const ast: ValidatedAst = Object.freeze({ nodes: result.document.nodes, height: result.height });
  admitted.add(ast);
  admittedSites.set(ast, counter.sites);
  return ast;
}
