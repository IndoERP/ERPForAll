/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Convergence property. The server model and the independent oracle live in
// ./model/server-model.ts; this file only generates scripts, runs them and compares with the oracle.
import { mkdirSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import fc from "fast-check";
import { expect, test } from "vitest";
import { type Action, type Mode, runScript } from "./model/server-model";

/** Fixed seed: CI replays exactly the same scripts every time. */
const SEED = 0x34a1c0de;
/** Runs per property. Bounded so the suite stays a few seconds. */
const NUM_RUNS = 300;
/** Longest script. Every run also starts with the forced T1/T2/answer pattern. */
const MAX_ACTIONS = 70;
/** Failing seeds are written here; `/logs/` is gitignored. */
const ARTIFACTS = resolve(process.cwd(), "../logs/store-wire-convergence");

const action = fc.record({
  kind: fc.integer({ min: 0, max: 13 }),
  a: fc.nat(1000),
  b: fc.nat(1000),
  c: fc.nat(1000),
});
const script = fc.array(action, { minLength: 20, maxLength: MAX_ACTIONS });

function violations(actions: readonly Action[], mode: Mode, breakA4 = false): string[] {
  return runScript(actions, { mode, breakA4 }).mismatches();
}

type Details = fc.RunDetails<[Action[]]>;

function check(mode: Mode, breakA4: boolean): Details {
  return fc.check(
    fc.property(script, (actions) => {
      const bad = violations(actions, mode, breakA4);
      if (bad.length > 0) throw new Error(bad.slice(0, 5).join("; "));
    }),
    { seed: SEED, numRuns: NUM_RUNS, endOnFailure: false, verbose: 0 },
  );
}

function record(name: string, details: Details): void {
  if (!details.failed) return;
  mkdirSync(ARTIFACTS, { recursive: true });
  const artifact = {
    property: name,
    seed: details.seed,
    shrinkPath: details.counterexamplePath,
    counterexample: details.counterexample,
    error: String(details.errorInstance),
  };
  writeFileSync(resolve(ARTIFACTS, `${name}.json`), `${JSON.stringify(artifact, null, 2)}\n`);
}

test("convergence: random interleavings of conflicting writers, snapshots, deletes, counters and memberships", () => {
  for (const mode of ["ordered", "arbitrary"] as const) {
    const details = check(mode, false);
    record(`convergence-${mode}`, details);
    if (details.failed) {
      throw new Error(`${mode}: ${String(details.errorInstance)} (seed ${details.seed}, path ${details.counterexamplePath})`, {
        cause: details.errorInstance,
      });
    }
  }
}, 120_000);

test("convergence negative control: an event for an unwritten coordinate breaks the property", () => {
  // A producer that breaks A4 must be caught; otherwise the property above could not fail at all.
  const details = check("ordered", true);
  expect(details.failed).toBe(true);
  expect(String(details.errorInstance)).toMatch(/state .*title: held=/);
}, 120_000);

test("convergence coverage: the generated runs exercise every kind of operation", () => {
  const total: Record<string, number> = {};
  for (const mode of ["ordered", "arbitrary"] as const) {
    for (const [i, [actions]] of fc.sample(fc.tuple(script), { seed: SEED, numRuns: NUM_RUNS }).entries()) {
      for (const [key, value] of Object.entries(runScript(actions, { mode }).coverage())) {
        const name = `${mode}.${key}`;
        total[name] = (total[name] ?? 0) + value;
      }
      expect(i).toBeGreaterThanOrEqual(0);
    }
  }
  // Record deletes only exist in ordered mode (see `Mode`); everything else must show up in both.
  for (const mode of ["ordered", "arbitrary"] as const) {
    for (const key of ["creates", "sets", "memberOps", "incs", "aborted", "frames", "answers", "writtenMarkers", "writingAnswers"]) {
      expect(total[`${mode}.${key}`] ?? 0, `${mode}.${key}`).toBeGreaterThan(0);
    }
  }
  expect(total["ordered.deletes"] ?? 0).toBeGreaterThan(0);
  expect(total["arbitrary.deletes"] ?? 0).toBe(0);
  expect(total["ordered.resyncs"] ?? 0, "capped counters reach a resync").toBeGreaterThan(0);
  console.info(`store-wire convergence coverage: ${JSON.stringify(total)}`);
}, 120_000);
