/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { expect, it } from "vitest";
import { decodeAst, parseBoundedJson, type JsonValue } from "../src/index";

it("admits every AST exported from the unchanged effective Rust golden table", () => {
  // Missing generated data is a failure; CI must run the exporter before Vitest.
  // The row count is pinned by crates/erp-expr/tests/goldens_tests.rs.
  const data = parseBoundedJson(readFileSync(resolve(process.cwd(), "packages/expr/tests/generated-goldens.json"), "utf8"));
  expect(Array.isArray(data)).toBe(true);
  const rows = data as ReadonlyArray<Record<string, JsonValue>>;
  expect(rows).toHaveLength(482);
  for (const [ordinal, row] of rows.entries()) {
    expect(row.ordinal).toBe(ordinal);
    expect(typeof row.source).toBe("string");
    expect(typeof row.evaluates).toBe("boolean");
    expect(typeof row.expected).toBe("string");
    const ast = decodeAst(row.ast);
    expect(ast.nodes.length).toBeGreaterThan(0);
    expect(ast.height).toBeGreaterThan(0);
    expect(ast.height).toBeLessThanOrEqual(100);
  }
});
