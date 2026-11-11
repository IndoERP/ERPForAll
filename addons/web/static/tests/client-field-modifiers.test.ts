/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import { combineFields, type Field } from "../src/client/metadata.js";
import { effectiveField } from "../src/client/field-modifiers.js";
import { decodeAst, parseBoundedJson } from "../../../../web/packages/expr/src/index.js";
const field: Field = { name: "name", kind: "char", label: "Name", readonly: false, required: false, relation: null, relationField: null, selection: null };
const boolean = (value: boolean) => decodeAst(parseBoundedJson(JSON.stringify({ schema: 1, nodes: [{ kind: "literal", value: { kind: "bool", value } }] })));
/// (partial: effective modifier evaluation and repeated occurrence combinations)
test("effective field occurrences retain AND readonly/invisible and OR required/force-save", () => {
  const first = effectiveField({ ...field, modifiers: { readonly: boolean(true), required: boolean(true), invisible: boolean(true) }, forceSave: true }, [field], {});
  const second = effectiveField({ ...field, modifiers: { readonly: boolean(false), required: boolean(false), invisible: boolean(false) } }, [field], {});
  expect(combineFields([first, second])[0]).toMatchObject({ readonly: false, required: true, invisible: false, forceSave: true });
});
/// (partial: modifier name failure is an explicit blocking validation error)
test("modifier evaluation failure never substitutes a boolean", () => {
  const expr = decodeAst(parseBoundedJson(JSON.stringify({ schema: 1, nodes: [{ kind: "name", name: "missing" }] })));
  expect(() => effectiveField({ ...field, modifiers: { readonly: expr, required: null, invisible: null } }, [field], {})).toThrowError(expect.objectContaining({ kind: "validation", message: "Cannot evaluate readonly for Name: NameError: name 'missing' is not defined" }));
});
