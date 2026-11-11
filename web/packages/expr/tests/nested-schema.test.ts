/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { TestOnlyResponse } from "../../../src/generated/TestOnlyResponse";
import { describe, expect, it } from "vitest";
import { AstDecodeError, decodeAst, DescriptionBudget, parseBoundedJson, type JsonValue } from "../src/index";

// This deliberately test-only owner uses the generated Rust root. Real view roots and
// their wrapper validators belong to WEB-SERVER-BASE.
function nestedResponse(text: string, maxExpressions = 1024) {
  const parsed = parseBoundedJson(text);
  function object(value: JsonValue | undefined, path: string, keys: string[]): Record<string, JsonValue> {
    if (value === null || typeof value !== "object" || Array.isArray(value)) throw new AstDecodeError(path, "expected object");
    for (const key of Object.keys(value)) if (!keys.includes(key)) throw new AstDecodeError(`${path}.${key}`, "unknown field");
    for (const key of keys) if (!Object.hasOwn(value, key)) throw new AstDecodeError(`${path}.${key}`, "missing required field");
    return value as Record<string, JsonValue>;
  }
  const response = object(parsed, "$", ["description"]);
  const description = object(response.description, "$.description", ["expressions"]);
  if (!Array.isArray(description.expressions)) throw new AstDecodeError("$.description.expressions", "expected array");
  const budget = new DescriptionBudget({ maxExpressions }, description);
  const expressions = description.expressions.map((value) => decodeAst(value, budget));
  // The Rust-generated type is checked here after this owner's complete validation.
  const wire: TestOnlyResponse = parsed as unknown as TestOnlyResponse;
  return { wire, expressions };
}

const ast = { schema: 1, nodes: [{ kind: "name", name: "x" }] };
function rejects(run: () => unknown, path: string, detail: string): void {
  const descriptors = Object.getOwnPropertyDescriptors(Object.prototype);
  const prototype = Object.getPrototypeOf(Object.prototype);
  const unrelated = {};
  let caught: unknown;
  try { run(); } catch (error) { caught = error; }
  expect(caught).toBeInstanceOf(AstDecodeError);
  expect(caught).toMatchObject({ kind: "InvalidAst", path, message: `InvalidAst at ${path}: ${detail}` });
  expect(Object.getOwnPropertyDescriptors(Object.prototype)).toEqual(descriptors);
  expect(Object.getPrototypeOf(Object.prototype)).toBe(prototype);
  expect(Object.getOwnPropertyDescriptors(unrelated)).toEqual({});
  expect(Object.getPrototypeOf(unrelated)).toBe(Object.prototype);
}
describe("Rust-owned test-only nested AST schema", () => {
  it("validates multiple ASTs from a single bounded response parse", () => {
    const result = nestedResponse(JSON.stringify({ description: { expressions: [ast, ast] } }));
    expect(result.wire.description.expressions).toHaveLength(2);
    expect(result.expressions.map((value) => value.height)).toEqual([1, 1]);
  });
  it("rejects wrapper duplicates before admitting any expression", () => {
    rejects(() => nestedResponse('{"description":{"expressions":[],"expressions":[]}}'), "$.description.expressions", "duplicate object key");
  });
  it("rejects a description aggregate instead of partially rendering", () => {
    rejects(() => nestedResponse(JSON.stringify({ description: { expressions: [ast, ast] } }), 1), "$.description.expressions[1]", "description expression limit exceeded (1)");
  });
});
