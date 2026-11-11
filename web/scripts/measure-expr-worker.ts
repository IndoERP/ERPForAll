/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Benchmark-only worker. validateStructure is internal and never authenticates public handles.
import { decodeAst, inspectValidated, validateStructure } from "../packages/expr/src/decoder";
import { inspectParsed, parseBoundedJson, type JsonValue } from "../packages/expr/src/reader";
import { admissionFixtures as fixtures } from "../packages/expr/bench/fixtures";
const PATHS = ["reader", "native_parse", "reader_decode", "native_parse_validate"] as const;
type Path = typeof PATHS[number];
let retained: unknown[] = [];
let checksum = 0;

function operation(text: string, path: Path): unknown {
  switch (path) {
    case "reader": return parseBoundedJson(text);
    case "native_parse": return JSON.parse(text) as JsonValue;
    case "reader_decode": return decodeAst(parseBoundedJson(text));
    case "native_parse_validate": return validateStructure(JSON.parse(text) as JsonValue);
  }
}
function fixtureAt(index: number) {
  const fixture = fixtures[index];
  if (!fixture) throw new Error(`unknown fixture ${index}`);
  return fixture;
}
function pathAt(path: string): Path {
  if (!PATHS.includes(path as Path)) throw new Error(`unknown path ${path}`);
  return path as Path;
}
function workload(index: number, selected: string, count: number, keep: boolean): number {
  if (!Number.isSafeInteger(count) || count < 1 || count > 10000) throw new Error("workload count must be 1..10000");
  const { text } = fixtureAt(index);
  const path = pathAt(selected);
  retained = [];
  for (let iteration = 0; iteration < count; iteration++) {
    const result = operation(text, path);
    if (keep) retained.push(result);
    // Each parser must run; retaining the final object prevents unused-result elimination.
    else if (iteration + 1 === count) retained.push(result);
    checksum += typeof result === "object" && result !== null ? 1 : 0;
  }
  return checksum;
}
// Pure metadata path shared with the benchmark: correctness tests need no clock or timing workload.
export function admissionMetadata(index: number, selected: string) {
  const fixture = fixtureAt(index);
  pathAt(selected);
  const parsed = parseBoundedJson(fixture.text);
  const admitted = decodeAst(parsed);
  const readerCounters = inspectParsed(parsed);
  const decoderCounters = inspectValidated(admitted);
  return {
    // All rows describe this fixture's combined admission, independently of the timing path.
    admissionCounterScope: "fixture-level reader+decode, independent of timed path",
    readerCounters, decoderCounters,
    admissionApplicationSites: readerCounters.applicationAllocations + decoderCounters.applicationAllocations,
    admissionApplicationSiteTarget: 8 + 6 * readerCounters.tokens,
  };
}

function benchmark(index: number, selected: string) {
  const fixture = fixtureAt(index);
  const path = pathAt(selected);
  const start = performance.now();
  workload(index, path, 1, false);
  const coldMicroseconds = (performance.now() - start) * 1000;
  workload(index, path, 256, false);
  const samples: number[] = [];
  const batch = 128;
  for (let sample = 0; sample < 32; sample++) {
    const began = performance.now();
    workload(index, path, batch, false);
    samples.push((performance.now() - began) * 1000 / batch);
  }
  const ordered = [...samples].sort((left, right) => left - right);
  const individualSamples: number[] = [];
  for (let sample = 0; sample < 512; sample++) {
    const began = performance.now();
    workload(index, path, 1, false);
    individualSamples.push((performance.now() - began) * 1000);
  }
  const individualOrdered = [...individualSamples].sort((left, right) => left - right);
  return {
    fixture: fixture.name, path, coldMicroseconds,
    timingUnit: "microseconds per operation, averaged over fixed batches",
    sampleCount: samples.length, operationsPerSample: batch,
    warmupOperations: 256, measuredOperations: batch * samples.length,
    p50: ordered[Math.ceil(ordered.length * 0.50) - 1],
    p99: ordered[Math.ceil(ordered.length * 0.99) - 1],
    samples,
    individualOperations: {
      unit: "microseconds per single operation, including workload loop/retention harness",
      sampleCount: individualSamples.length,
      p50: individualOrdered[Math.ceil(individualOrdered.length * 0.50) - 1],
      p99: individualOrdered[Math.ceil(individualOrdered.length * 0.99) - 1],
      zeroDurationSamples: individualSamples.filter((sample) => sample === 0).length,
      samples: individualSamples,
    },
    ...admissionMetadata(index, selected),
    checksum,
  };
}

Object.assign(globalThis, {
  exprMeasurement: {
    fixtures: fixtures.map((fixture) => ({ name: fixture.name, encodedBytes: new TextEncoder().encode(fixture.text).length, wire: fixture.text })),
    paths: PATHS,
    environment() {
      const positiveTicksMicroseconds: number[] = [];
      let previous = performance.now();
      let probes = 0;
      for (; probes < 100000 && positiveTicksMicroseconds.length < 64; probes++) {
        const current = performance.now();
        if (current > previous) positiveTicksMicroseconds.push((current - previous) * 1000);
        previous = current;
      }
      return { crossOriginIsolated: globalThis.crossOriginIsolated, timeOrigin: performance.timeOrigin,
        timerPrecision: { probes, positiveTicksMicroseconds,
          minimumObservedPositiveTickMicroseconds: positiveTicksMicroseconds.length ? Math.min(...positiveTicksMicroseconds) : null,
          limitation: "observed clock increments, not a guarantee of timer resolution or scheduling precision" } };
    },
    benchmark, workload,
    release() { retained = []; return checksum; },
  },
});
