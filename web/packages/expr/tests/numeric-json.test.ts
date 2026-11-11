/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import { parseBoundedJson, readResponseJson } from "../src/index.js";
/// (partial: record JSON refuses decimal precision loss before Number discards the original token)
test("exact record number admission refuses lost lexical precision", () => {
  for (const source of ['{"amount":1.00000000000000001}', '{"amount":9007199254740993}', '{"amount":90071992547409.91}', '{"amount":1e-999}']) {
    expect(() => parseBoundedJson(source, {}, "exact_decimal")).toThrowError(expect.objectContaining({ kind: "InvalidAst", message: "InvalidAst at $.amount: number cannot round-trip through the record decimal contract" }));
  }
});
/// (partial: exact decimal admission retains ordinary numbers and keeps generic AST parsing separate)
test("exact decimal admission preserves safe values including exponents", async () => {
  expect(parseBoundedJson('[0,0.1,-12.50,1e3,9007199254740991]', {}, "exact_decimal")).toEqual([0, 0.1, -12.5, 1000, 9007199254740991]);
  expect(await readResponseJson(new Response('{"amount":0.125}'), {}, "exact_decimal")).toEqual({ amount: 0.125 });
  expect(parseBoundedJson('{"amount":1.00000000000000001}')).toEqual({ amount: 1 });
});
