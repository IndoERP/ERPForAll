/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { createContext, evalAst, isTruthy, type ExprValue } from "../../../../../web/packages/expr/src/index.js";
import { recordExpressionValue } from "./buttons.js";
import type { Field, Node } from "./metadata.js";
import { ClientError } from "./transport.js";
/** Evaluate policy using one record snapshot. Failures block validation and are returned to the owner. */
export function effectiveField(field: Field, fields: readonly Field[], values: Readonly<Record<string, unknown>>): Field {
  if (!field.modifiers) return field;
  const names: Record<string, ExprValue> = Object.create(null);
  for (const item of fields) names[item.name] = recordExpressionValue(item, values[item.name]);
  const effective = { ...field };
  for (const key of ["readonly", "required", "invisible"] as const) {
    const expression = field.modifiers[key];
    if (!expression) continue;
    try { effective[key] = isTruthy(evalAst(expression, createContext({ names }))) || key === "readonly" && field.readonly; }
    catch (error) { throw new ClientError("validation", `Cannot evaluate ${key} for ${field.label}: ${error instanceof Error ? error.message : String(error)}`); }
  }
  if (field.name === "id") effective.readonly = true;
  return effective;
}
/** Keep occurrence presentation separate from combined record policy, including nested layout regions. */
export function effectiveNodes(nodes: readonly Node[], fields: readonly Field[], values: Readonly<Record<string, unknown>>): Node[] {
  return nodes.map(node => node.kind === "field" ? { ...node, field: effectiveField(node.field, fields, values) } : "children" in node ? { ...node, children: effectiveNodes(node.children, fields, values) } : node);
}
export function occurrenceFields(nodes: readonly Node[]): Field[] {
  return nodes.flatMap(node => node.kind === "field" ? node.widget === "relation_count" ? [] : [node.field] : "children" in node ? occurrenceFields(node.children) : []);
}
