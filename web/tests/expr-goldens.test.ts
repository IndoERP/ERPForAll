/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The whole original golden table is mandatory through both entry points, independently
// of the historical child ownership manifest. Expectations are never produced by our code.
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";
import {
  PythonError, boolVal, createContext, decodeAst, evalAst, parseBoundedJson, repr,
  type ExprValue,
} from "@erpforall/expr";
import { signal } from "../src/runtime/reactive.js";
import { createRecordExpression, DisposedExpression } from "../src/runtime/expr.js";

interface GoldenRow {
  readonly ordinal: number;
  readonly source: string;
  readonly evaluates: boolean;
  readonly expected: string;
  readonly ast: unknown;
}

const rows = parseBoundedJson(readFileSync(resolve(process.cwd(),
  "packages/expr/tests/generated-goldens.json"), "utf8")) as unknown as readonly GoldenRow[];

type Outcome = { readonly ok: true; readonly value: ExprValue; readonly text: string }
  | { readonly ok: false; readonly error: unknown };

function outcome(read: () => ExprValue): Outcome {
  try {
    const value = read();
    return { ok: true, value, text: repr(value) };
  } catch (error) {
    return { ok: false, error };
  }
}

function expectAssertion(read: () => void): void {
  let failure: unknown;
  try { read(); } catch (error) { failure = error; }
  expect(failure).toBeInstanceOf(Error);
  // Framework-rendered value diffs vary; the failure must be an assertion,
  // rather than a TypeError from broken test setup.
  expect(failure).toMatchObject({ name: "AssertionError" });
}

function assertGolden(row: GoldenRow, actual: Outcome): void {
  expect(actual.ok, `row=${row.ordinal} source=${row.source}`).toBe(row.evaluates);
  if (actual.ok) {
    expect(actual.text).toBe(row.expected);
  } else {
    expect(actual.error).toBeInstanceOf(PythonError);
    const error = actual.error as PythonError;
    // Assert both the typed diagnostic and its complete original-table text.
    expect(error.className).toBe(row.expected.slice(0, row.expected.indexOf(":")));
    expect(error.message).toBe(row.expected);
    expect(error.toString()).toBe(row.expected);
  }
}

it("requires the complete contiguous original table, without ownership filters", () => {
  expect(rows).toHaveLength(482);
  for (const [ordinal, row] of rows.entries()) {
    expect(Object.keys(row).sort()).toEqual(["ast", "evaluates", "expected", "ordinal", "source"]);
    expect(row.ordinal).toBe(ordinal);
    expect(typeof row.source).toBe("string");
    expect(typeof row.evaluates).toBe("boolean");
    expect(typeof row.expected).toBe("string");
    expect(row.source.length).toBeGreaterThan(0);
    expect(row.expected.length).toBeGreaterThan(0);
  }
});

for (const row of rows) {
  describe(`original golden ${row.ordinal}: ${row.source}`, () => {
    it("evaluates through the pure client", () => {
      const ast = decodeAst(row.ast);
      assertGolden(row, outcome(() => evalAst(ast, createContext())));
    });

    it("invalidates and recovers across a lazy signal branch for this operation family", () => {
      const original = decodeAst(row.ast);
      const n = original.nodes.length;
      // Separate integration wrapper; neither the original row nor its expectations change.
      const ast = decodeAst(parseBoundedJson(JSON.stringify({ schema: 1, nodes: [...original.nodes,
        { kind: "name", name: "golden_enabled" },
        { kind: "literal", value: { kind: "none" } },
        { kind: "if_exp", test: n, body: n - 1, orelse: n + 1 },
      ] })));
      const enabled = signal(boolVal(true));
      let reads = 0;
      const handle = createRecordExpression(ast, {
        id: false,
        record: (name) => {
          if (name !== "golden_enabled") return undefined;
          reads++;
          return enabled();
        },
        session: { uid: 1n, allowed_company_ids: [1n], context: {} },
      });
      try {
        expect(reads).toBe(0);
        assertGolden(row, outcome(() => handle.read));
        assertGolden(row, outcome(() => handle.read));
        expect(reads).toBe(1);
        enabled.set(boolVal(false));
        expect(repr(handle.read)).toBe("None");
        expect(repr(handle.read)).toBe("None");
        expect(reads).toBe(2);
        enabled.set(boolVal(true));
        assertGolden(row, outcome(() => handle.read));
        assertGolden(row, outcome(() => handle.read));
        expect(reads).toBe(3);
      } finally {
        handle.dispose();
      }
    });

    it("evaluates through the reactive adapter, caches repeat reads and disposes", () => {
      const ast = decodeAst(row.ast);
      const handle = createRecordExpression(ast, {
        id: false,
        session: { uid: 1n, allowed_company_ids: [1n], context: {} },
      });
      try {
        const first = outcome(() => handle.read);
        assertGolden(row, first);
        const repeated = outcome(() => handle.read);
        assertGolden(row, repeated);
        if (first.ok && repeated.ok) expect(repeated.value).toBe(first.value);
        if (!first.ok && !repeated.ok) expect(repeated.error).toBe(first.error);
      } finally {
        handle.dispose();
      }
      expect(handle.isDisposed).toBe(true);
      let disposed: unknown;
      try { void handle.read; } catch (error) { disposed = error; }
      expect(disposed).toBeInstanceOf(DisposedExpression);
      expect(disposed).toMatchObject({ code: "disposed", message: "expression handle has been disposed" });
      handle.dispose();
    });
  });
}

it("rejects mutated success, error and outcome expectations using actual evaluator output", () => {
  const success = rows.find((row) => row.evaluates);
  const failure = rows.find((row) => !row.evaluates);
  expect(success).toBeDefined();
  expect(failure).toBeDefined();
  for (const row of [success!, failure!]) {
    const actual = outcome(() => evalAst(decodeAst(row.ast), createContext()));
    assertGolden(row, actual);
    expectAssertion(() => assertGolden({ ...row, expected: "deliberately wrong expectation" }, actual));
    expectAssertion(() => assertGolden({ ...row, evaluates: !row.evaluates }, actual));
  }
});
