/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import {
  createStructuredSourceEditor,
  type DomainEditorHandle,
  type DomainEditorOptions,
} from "./domain-editor.js";
import type { Field } from "./metadata.js";
/** Local exclusion descriptor; properties never widen generated server metadata. */
export type ExpressionField = Field | {
  readonly name: string;
  readonly label: string;
  readonly kind: "properties";
};
/** Caller-supplied root fields only; expressions never traverse related models. */
export interface ExpressionEditorOptions
  extends Omit<DomainEditorOptions, "fields"> {
  fields(
    model: string,
    signal: AbortSignal,
  ):
    | ReadonlyMap<string, ExpressionField>
    | Promise<ReadonlyMap<string, ExpressionField>>;
}
export type ExpressionEditorHandle = DomainEditorHandle;
/** Build a source authoring editor, without evaluation or RPC. */
export function createExpressionEditor(
  options: ExpressionEditorOptions,
): ExpressionEditorHandle {
  return createStructuredSourceEditor(options, "expression");
}
