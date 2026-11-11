/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { expect, it } from "vitest";
import { decodeAst, isValidatedAst, parseBoundedJson, type JsonValue } from "../src/index";

function object(value: JsonValue | undefined, keys: string[]): Record<string, JsonValue> {
  expect(value).not.toBe(null);
  expect(typeof value).toBe("object");
  expect(Array.isArray(value)).toBe(false);
  const result = value as Record<string, JsonValue>;
  expect(Object.keys(result).sort()).toEqual([...keys].sort());
  return result;
}

it("preserves the separate Rust differential oracle and admits every successfully parsed AST", () => {
  // Transport checks complement differential-property.test.ts actual evaluation.
  const parsed = parseBoundedJson(readFileSync(resolve(process.cwd(), "packages/expr/tests/generated-differential.json"), "utf8"));
  const corpus = object(parsed, ["kind", "seed", "context", "limits", "rows"]);
  expect(corpus.kind).toBe("generated_rust_differential");
  expect(corpus.seed).toBe(0x55495801);
  expect(corpus.context).toBe("EvalContext::standard; no host vars; no clock");
  expect(corpus.limits).toEqual({
    max_source_bytes: 65536, max_depth: 100, max_steps: 1000000,
    max_value_bytes: 1048576, max_total_bytes: 16777216, max_format_width: 1000,
  });
  expect(Array.isArray(corpus.rows)).toBe(true);
  const rows = corpus.rows as readonly JsonValue[];
  expect(rows).toHaveLength(6000);
  let parsedRows = 0;
  let rejectedRows = 0;
  for (const [ordinal, rawRow] of rows.entries()) {
    const descriptors = Object.getOwnPropertyDescriptors(Object.prototype);
    const prototype = Object.getPrototypeOf(Object.prototype);
    const unrelated = {};
    try {
      const row = object(rawRow, ["ordinal", "family", "case", "seed", "source", "outcome"]);
      expect(row.ordinal).toBe(ordinal);
      expect(typeof row.source).toBe("string");
      expect(new TextEncoder().encode(row.source as string).byteLength).toBeLessThanOrEqual(256);
      // The parser family remains an explicit outcome, never a silent skip.
      if ((row.outcome as Record<string, JsonValue>).status === "parse_error") {
        const outcome = object(row.outcome, ["status", "class", "message"]);
        expect(outcome.status).toBe("parse_error");
        expect(outcome.class).toBe("SyntaxError");
        expect(outcome.message).toMatch(/^SyntaxError: .+ \(line \d+, column \d+\)$/);
        rejectedRows++;
      } else {
        const outcome = object(row.outcome, ["status", "ast", "result"]);
        expect(outcome.status).toBe("parsed");
        const ast = decodeAst(outcome.ast);
        expect(isValidatedAst(ast)).toBe(true);
        expect(ast.nodes.length).toBeGreaterThan(0);
        expect(ast.height).toBeGreaterThan(0);
        expect(ast.height).toBeLessThanOrEqual(100);
        const result = outcome.result as Record<string, JsonValue>;
        expect(result.status === "ok" || result.status === "error").toBe(true);
        if (result.status === "ok") {
          object(result, ["status", "repr"]);
          expect(typeof result.repr).toBe("string");
        } else {
          object(result, ["status", "class", "message"]);
          expect(typeof result.class).toBe("string");
          expect((result.class as string).length).toBeGreaterThan(0);
          expect(typeof result.message).toBe("string");
          expect((result.message as string).startsWith(`${result.class}: `)).toBe(true);
        }
        parsedRows++;
      }
    } finally {
      expect(Object.getOwnPropertyDescriptors(Object.prototype)).toEqual(descriptors);
      expect(Object.getPrototypeOf(Object.prototype)).toBe(prototype);
      expect(Object.getOwnPropertyDescriptors(unrelated)).toEqual({});
      expect(Object.getPrototypeOf(unrelated)).toBe(Object.prototype);
    }
  }
  expect(parsedRows + rejectedRows).toBe(rows.length);
  expect(parsedRows).toBeGreaterThan(0);
  expect(rejectedRows).toBeGreaterThan(0);
});
