/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { deepStrictEqual } from "node:assert";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import fc from "fast-check";
import { expect, it } from "vitest";
import { AST_LIMITS, RESPONSE_LIMITS, AstDecodeError, decodeAst, parseBoundedJson } from "../src/index";
import { withPrototypeInvariant } from "./support/differential";

interface Replay { wire: string; path: string; detail: string; }
const SEED = 0x55495831;
const LIMITS = { reader: RESPONSE_LIMITS, decoder: AST_LIMITS };
function assertHostile(sample: Replay): void {
  withPrototypeInvariant(() => {
    let failure: unknown;
    try { decodeAst(parseBoundedJson(sample.wire)); }
    catch (error) { failure = error; }
    expect(failure).toBeInstanceOf(AstDecodeError);
    expect(failure).toMatchObject({ kind: "InvalidAst", path: sample.path,
      message: `InvalidAst at ${sample.path}: ${sample.detail}` });
  });
}
const replayPath = resolve(process.cwd(), "packages/expr/tests/fuzz-replays.json");
const replays = JSON.parse(readFileSync(replayPath, "utf8")) as { seed: number; shrinkPath: string; sample: Replay }[];
it("replays committed hostile regression fixtures read-only", () => {
  expect(replays.length).toBeGreaterThan(0);
  for (const replay of replays) assertHostile(replay.sample);
});
it("shrinks seeded hostile fields and indices with exact diagnostics and prototype integrity", () => {
  const arbitrary = fc.tuple(fc.integer({ min: 0, max: 3 }), fc.integer({ min: 0, max: 1000 })).map(([kind, index]): Replay => {
    switch (kind) {
      case 0: return { wire: `{"schema":1,"nodes":[{"kind":"starred","value":${index}}]}`, path: "$.nodes[0]", detail: `child index ${index} must point strictly backwards` };
      case 1: return { wire: `{"schema":1,"nodes":[],"schema":${index}}`, path: "$.schema", detail: "duplicate object key" };
      case 2: return { wire: `{"schema":1,"nodes":[{"kind":"name","name":"n${index}","__proto__":{"polluted":true}}]}`, path: '$.nodes[0]["__proto__"]', detail: "unknown field" };
      default: return { wire: `{"schema":1,"nodes":[{"kind":"literal","value":{"kind":"int","value":"00${index}"}}]}`, path: "$.nodes[0].value.value", detail: "expected canonical i64 decimal string" };
    }
  });
  const details = fc.check(fc.property(arbitrary, assertHostile), { seed: SEED, numRuns: 1000 });
  if (details.failed) {
    if (details.counterexample === null) throw new Error("property interrupted without a counterexample", { cause: details.errorInstance });
    const dir = resolve(process.cwd(), "../logs/uix-fuzz");
    mkdirSync(dir, { recursive: true });
    const artifact = { seed: details.seed, shrinkPath: details.counterexamplePath,
      limits: LIMITS, context: "decoder only; no host variables or clock",
      sample: details.counterexample[0], error: String(details.errorInstance) };
    writeFileSync(resolve(dir, "hostile.json"), JSON.stringify(artifact, null, 2) + "\n");
    throw new Error(`hostile property failed seed=${details.seed} path=${details.counterexamplePath}`, { cause: details.errorInstance });
  }
  deepStrictEqual(details.failed, false);
});
