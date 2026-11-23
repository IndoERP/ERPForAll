/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { createContext, evalAst, isTruthy, type ExprValue, type ValidatedAst } from "../../../../../web/packages/expr/src/index.js";
import { recordExpressionValue } from "./buttons.js";
import type { Field, Node, PageNode } from "./metadata.js";
import { ClientError } from "./transport.js";
/** Evaluate one modifier expression on a record snapshot: the names it reads are the record's fields. */
function truthy(expression: ValidatedAst, fields: readonly Field[], values: Readonly<Record<string, unknown>>): boolean {
  const requested = new Set(expression.nodes.filter(node => node.kind === "name").map(node => node.name));
  const names: Record<string, ExprValue> = Object.create(null);
  for (const item of fields) {
    if (requested.has(item.name)) names[item.name] = recordExpressionValue(item, values[item.name]);
  }
  return isTruthy(evalAst(expression, createContext({ names })));
}
/** Evaluate policy using one record snapshot. Failures block validation and are returned to the owner. */
export function effectiveField(field: Field, fields: readonly Field[], values: Readonly<Record<string, unknown>>): Field {
  if (!field.modifiers) return field;
  const effective = { ...field };
  for (const key of ["readonly", "required", "invisible"] as const) {
    const expression = field.modifiers[key];
    if (!expression) continue;
    try { effective[key] = truthy(expression, fields, values) || key === "readonly" && field.readonly; }
    catch (error) { throw new ClientError("validation", `Cannot evaluate ${key} for ${field.label}: ${error instanceof Error ? error.message : String(error)}`); }
  }
  if (field.name === "id") effective.readonly = true;
  return effective;
}
/** Keep occurrence presentation separate from combined record policy, including nested layout regions. */
export function effectiveNodes(nodes: readonly Node[], fields: readonly Field[], values: Readonly<Record<string, unknown>>): Node[] {
  return nodes.map(node => node.kind === "field" ? { ...node, field: effectiveField(node.field, fields, values) }
    : node.kind === "page" ? { ...node, hidden: pageHidden(node, fields, values), children: effectiveNodes(node.children, fields, values) }
    : "children" in node ? { ...node, children: effectiveNodes(node.children, fields, values) } : node);
}
/** A page hides per record while its `invisible` expression is true; the shape of the node tree is kept. */
function pageHidden(page: PageNode, fields: readonly Field[], values: Readonly<Record<string, unknown>>): boolean {
  if (!page.invisible) return false;
  try { return truthy(page.invisible, fields, values); }
  catch (error) { throw new ClientError("validation", `Cannot evaluate invisible for the page ${page.label}: ${error instanceof Error ? error.message : String(error)}`); }
}
export function occurrenceFields(nodes: readonly Node[]): Field[] {
  return nodes.flatMap(node => node.kind === "field" ? node.widget === "relation_count" ? [] : [node.field] : "children" in node ? occurrenceFields(node.children) : []);
}
