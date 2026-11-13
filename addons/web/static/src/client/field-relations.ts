/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { createContext, decodeAst, evalAst, parseBoundedJson, FALSE, NONE, boolVal, strVal, floatVal, intVal, listVal, dictVal, type ExprValue } from "../../../../../web/packages/expr/src/index.js";
import type { Field } from "./metadata.js";
import { ClientError } from "./transport.js";
export interface ConversionBudget { nodes: number; bytes: number }
export function expressionValue(value: unknown, depth = 0, budget: ConversionBudget = { nodes: 0, bytes: 0 }): ExprValue {
  budget.nodes++;
  if (typeof value === "string") budget.bytes += new TextEncoder().encode(value).length;
  if (budget.nodes > 4096 || budget.bytes > 262144) throw new ClientError("validation", "Relation expression inputs exceed 4096 nodes or 262144 bytes.");
  if (depth > 16) throw new ClientError("validation", "Relation context nesting exceeds 16 levels.");
  if (value === null || value === undefined) return NONE;
  if (value === false) return FALSE;
  if (typeof value === "boolean") return boolVal(value);
  if (typeof value === "string") return strVal(value);
  if (typeof value === "number" && Number.isFinite(value)) return Number.isSafeInteger(value) ? intVal(value) : floatVal(value);
  if (Array.isArray(value) && value.length <= 4096) return listVal(value.map(item => expressionValue(item, depth + 1, budget)));
  if (typeof value === "object" && value !== null && Object.keys(value).length <= 4096) {
    if ("id" in value && typeof value.id === "number" && Number.isSafeInteger(value.id)) return intVal(value.id);
    return dictVal(Object.entries(value).map(([key, item]) => { budget.bytes += new TextEncoder().encode(key).length; return [strVal(key), expressionValue(item, depth + 1, budget)] as const; }));
  }
  throw new ClientError("validation", "Unsupported relation expression value.");
}
export function jsonValue(value: ExprValue, depth = 0): unknown {
  if (depth > 16) throw new ClientError("validation", "Relation expression result exceeds 16 levels.");
  switch (value.kind) {
    case "none": return null;
    case "bool": case "str": return value.value;
    case "float": if (!Number.isFinite(value.value)) throw new ClientError("validation", "Relation expression returned a nonfinite number."); return value.value;
    case "int": { const number = Number(value.value); if (!Number.isSafeInteger(number)) throw new ClientError("validation", "Relation expression returned an unsafe integer."); return number; }
    case "tuple": case "list": return value.items.map(item => jsonValue(item, depth + 1));
    case "dict": { const result: Record<string, unknown> = Object.create(null); for (const [key, item] of value.entries) { if (key.kind !== "str") throw new ClientError("validation", "Relation context keys must be text."); result[key.value] = jsonValue(item, depth + 1); } return result; }
    default: throw new ClientError("validation", "Relation expression returned an unsupported value.");
  }
}
/**
 *  A context that reads record values is
 *  per row; reads do not apply it yet (pickers do, through `relationQuery`). 
 */
export function constantRelationContext(field: Field): Record<string, unknown> | undefined {
  if (!field.relation || !field.options?.context) return undefined;
  const ast = decodeAst(parseBoundedJson(JSON.stringify(field.options.context)));
  if (ast.nodes.some(node => node.kind === "name")) return undefined;
  const value = jsonValue(evalAst(ast, createContext({ names: Object.create(null) })));
  if (value === null || typeof value !== "object" || Array.isArray(value)) throw new ClientError("validation", "Relation context must evaluate to a dictionary.");
  return value as Record<string, unknown>;
}
/** Effective domain and context are shared by autocomplete and the existing expanded picker. */
export function relationQuery(field: Field, values: Readonly<Record<string, unknown>>, context: Record<string, unknown>, fields: readonly Field[] = [], parts: "both" | "context" = "both"): { domain: unknown[]; context: Record<string, unknown> } {
  const documents = {
    // "context" leaves the domain unread: a caller that never searches must not fail on a domain it does not use.
    domain: parts === "both" && field.options?.domain ? decodeAst(parseBoundedJson(JSON.stringify(field.options.domain))) : undefined,
    context: field.options?.context ? decodeAst(parseBoundedJson(JSON.stringify(field.options.context))) : undefined,
  };
  const needed = new Set<string>();
  for (const document of Object.values(documents)) for (const node of document?.nodes ?? []) if (node.kind === "name") needed.add(node.name);
  const names: Record<string, ExprValue> = Object.create(null);
  const budget = { nodes: 0, bytes: 0 };
  for (const name of needed) {
    if (name === "context") names[name] = expressionValue(context, 0, budget);
    else if (Object.hasOwn(values, name)) names[name] = expressionValue(values[name], 0, budget);
    else if (fields.some(field => field.name === name)) names[name] = NONE;
  }
  const evaluate = (ast: NonNullable<typeof documents.domain>): unknown => jsonValue(evalAst(ast, createContext({ names })));
  const domain = documents.domain ? evaluate(documents.domain) : [];
  const extra = documents.context ? evaluate(documents.context) : {};
  if (!Array.isArray(domain)) throw new ClientError("validation", "Relation domain must evaluate to a list.");
  if (extra === null || typeof extra !== "object" || Array.isArray(extra)) throw new ClientError("validation", "Relation context must evaluate to a dictionary.");
  if (Object.hasOwn(extra, "group_by")) throw new ClientError("unsupported", "Relation group_by context is not supported.");
  return { domain, context: { ...context, ...extra } };
}
