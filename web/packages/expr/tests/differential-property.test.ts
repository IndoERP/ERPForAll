/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { AssertionError } from "node:assert";
import { describe, expect, it } from "vitest";
import { parseBoundedJson } from "../src/index";
import { assertRow, checkRows, families, loadCorpus, SEED, type Row } from "./support/differential";

const corpus = loadCorpus();
describe("seeded Rust differential evaluation", () => {
  it("requires all configured families, seeds, cases and bounded sources", () => {
    expect(corpus.kind).toBe("generated_rust_differential");
    expect(corpus.seed).toBe(SEED);
    expect(corpus.context).toBe("EvalContext::standard; no host vars; no clock");
    expect(corpus.rows).toHaveLength(6000);
    expect([...new Set(corpus.rows.map(row => row.family))].sort()).toEqual([...families].sort());
    for (const family of families) {
      const rows = corpus.rows.filter(row => row.family === family);
      expect(rows).toHaveLength(1000);
      for (const [index, row] of rows.entries()) {
        expect(row.seed).toBe(SEED);
        expect(row.case).toBe(index);
        expect(row.ordinal).toBe(corpus.rows.indexOf(row));
        expect(new TextEncoder().encode(row.source).length).toBeLessThanOrEqual(256);
        expect(["parsed", "parse_error"]).toContain(row.outcome.status);
        if (["containers", "format", "dates"].includes(family)) {
          expect(row.outcome.status).toBe("parsed");
          if (row.outcome.status === "parsed") expect(row.outcome.result.status).toBe("ok");
        }
      }
    }
  });
  it("replays minimized harness counterexamples read-only with original Rust outcomes", () => {
    const replays = parseBoundedJson(readFileSync(resolve(process.cwd(), "packages/expr/tests/differential-replays.json"), "utf8")) as unknown as { row: Row }[];
    expect(replays).toHaveLength(7);
    for (const replay of replays) assertRow(replay.row);
  });
  for (const family of families) {
    it(`compares and exhaustively replays ${family} with bounded property shrinking`, () => checkRows(corpus, family), 60_000);
  }
  it("refuses corrupt repr, full error text, class and outcome discriminator", () => {
    const good = corpus.rows.find(row => row.outcome.status === "parsed" && row.outcome.result.status === "ok")!;
    const bad = corpus.rows.find(row => row.outcome.status === "parsed" && row.outcome.result.status === "error")!;
    function mutated(row: Row, result: unknown): Row {
      // Re-enter the bounded reader so mutation tests preserve provenance.
      return parseBoundedJson(JSON.stringify({ ...row, outcome: { ...row.outcome, result } })) as unknown as Row;
    }
    expect(() => assertRow(mutated(good, { status: "ok", repr: "WRONG" }))).toThrow(AssertionError);
    expect(() => assertRow(mutated(bad, { status: "error", class: "WRONG", message: "WRONG" }))).toThrow(AssertionError);
    if (bad.outcome.status !== "parsed" || bad.outcome.result.status !== "error") throw new Error("missing error row");
    const errorResult = bad.outcome.result;
    expect(() => assertRow(mutated(bad, { ...errorResult, message: "WRONG" }))).toThrow(AssertionError);
    expect(() => assertRow(mutated(bad, { ...errorResult, class: "WRONG" }))).toThrow(AssertionError);
    expect(() => assertRow(mutated(good, { status: "error", class: "WRONG", message: "WRONG" }))).toThrow(AssertionError);
  });
});
