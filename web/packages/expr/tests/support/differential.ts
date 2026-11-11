/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { deepStrictEqual } from "node:assert";
import { readFileSync, mkdirSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import fc from "fast-check";
import { createContext, decodeAst, evalAst, parseBoundedJson, PythonError, repr } from "../../src/index";

export const families = ["core", "containers", "format", "dates", "errors", "parser"] as const;
export const SEED = 0x55495801;
export type Result = { status: "ok"; repr: string } | { status: "error"; class: string; message: string };
export interface Row {
  ordinal: number; family: string; case: number; seed: number; source: string;
  outcome: { status: "parsed"; ast: unknown; result: Result } | { status: "parse_error"; class: string; message: string };
}
export interface Corpus { kind: string; seed: number; context: string; limits: unknown; rows: Row[]; }
export function loadCorpus(): Corpus {
  return parseBoundedJson(readFileSync(resolve(process.cwd(), "packages/expr/tests/generated-differential.json"), "utf8")) as unknown as Corpus;
}

/** Every generated/hostile evaluation restores evidence even when evaluation throws. */
export function withPrototypeInvariant(action: () => void): void {
  const descriptors = Object.getOwnPropertyDescriptors(Object.prototype);
  const prototype = Object.getPrototypeOf(Object.prototype);
  const unrelated = {};
  try { action(); }
  finally {
    deepStrictEqual(Object.getOwnPropertyDescriptors(Object.prototype), descriptors);
    deepStrictEqual(Object.getPrototypeOf(Object.prototype), prototype);
    deepStrictEqual(Object.getOwnPropertyDescriptors(unrelated), {});
    deepStrictEqual(Object.getPrototypeOf(unrelated), Object.prototype);
  }
}
export function assertRow(row: Row): void {
  withPrototypeInvariant(() => {
    if (row.outcome.status === "parse_error") {
      // There is no source parser in the browser. Rust harness replays this exact outcome;
      // retain its class/full text here instead of dropping it or pretending TS parsed it.
      deepStrictEqual(row.outcome.class, "SyntaxError");
      if (!/^SyntaxError: .+ \(line \d+, column \d+\)$/.test(row.outcome.message)) {
        throw new Error(`invalid parser outcome: ${row.outcome.message}`);
      }
      return;
    }
    const ast = decodeAst(row.outcome.ast);
    let actual: Result;
    try { actual = { status: "ok", repr: repr(evalAst(ast, createContext())) }; }
    catch (error) {
      if (!(error instanceof PythonError)) throw error;
      actual = { status: "error", class: error.className, message: error.toString() };
    }
    deepStrictEqual(actual, { ...row.outcome.result }, `family=${row.family} case=${row.case} source=${row.source}`);
  });
}

/** Finite corpus shrink: minimizes the failing row index, not the source grammar. */
export function checkRows(corpus: Corpus, family: string): void {
  const rows = corpus.rows.filter(row => row.family === family);
  deepStrictEqual(rows.length, 1000);
  const failures: { row: Row; error: string }[] = [];
  for (const row of rows) {
    try { assertRow(row); }
    catch (error) { failures.push({ row, error: String(error) }); }
  }
  const details = fc.check(fc.property(fc.integer({ min: 0, max: rows.length - 1 }), index => {
    assertRow(rows[index]!);
  }), { seed: SEED, numRuns: 1000, endOnFailure: false });
  if (details.failed || failures.length > 0) {
    if (details.failed && details.counterexample === null) throw new Error("property interrupted without a counterexample", { cause: details.errorInstance });
    const minimized = details.counterexample === null ? failures[0]!.row : rows[details.counterexample[0]]!;
    const artifact = {
      kind: "generated_rust_differential_failure", propertySeed: details.seed,
      shrinkPath: details.counterexamplePath, shrinkDomain: "corpus row index",
      context: corpus.context, limits: corpus.limits, row: minimized,
      failures, error: String(details.errorInstance),
    };
    const dir = resolve(process.cwd(), "../logs/uix-fuzz");
    mkdirSync(dir, { recursive: true });
    const path = resolve(dir, `differential-${family}.json`);
    writeFileSync(path, JSON.stringify(artifact, null, 2) + "\n");
    throw new Error(`differential failure; replay artifact=${path} seed=${details.seed} path=${details.counterexamplePath}`, { cause: details.errorInstance });
  }
}
