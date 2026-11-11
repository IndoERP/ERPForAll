/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Compare recorded measurements; never turn absent/invalid data into a passing value.
type Data = Record<string, unknown>;
function object(value: unknown, label: string): Data {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new Error(`invalid measurement: ${label} must be an object`);
  }
  return value as Data;
}
function list(value: unknown, label: string): unknown[] {
  if (!Array.isArray(value)) throw new Error(`invalid measurement: ${label} must be an array`);
  return value;
}
function text(value: unknown, label: string): string {
  if (typeof value !== "string" || value.length === 0) throw new Error(`invalid measurement: ${label} must be a nonempty string`);
  return value;
}
function positive(value: unknown, label: string): number {
  if (typeof value !== "number" || !Number.isFinite(value) || value <= 0) {
    throw new Error(`invalid measurement: ${label} must be finite and positive`);
  }
  return value;
}
function rows(value: unknown): Map<string, Data> {
  const result = new Map<string, Data>();
  for (const raw of list(value, "rows")) {
    const row = object(raw, "row");
    const key = `${text(row.fixture, "row.fixture")}/${text(row.path, "row.path")}`;
    if (result.has(key)) throw new Error(`invalid measurement: duplicate ${key}`);
    result.set(key, row);
  }
  return result;
}

/** Warm batch-average timing and explicit allocation-site ratchets, without tolerance.
 * Physical peak heap is not measured by this harness and is not inferred here.
 * Callers must preserve the complete new output even when comparison rejects it.
 */
export function checkAdmissionRatchet(reference: unknown, measured: unknown): string[] {
  const baseline = object(reference, "baseline");
  const candidate = object(measured, "candidate");
  if (baseline.schema !== 1 || candidate.schema !== 1) throw new Error("invalid measurement: expected schema 1");
  for (const [group, fields] of [
    ["tools", ["chrome", "node", "esbuild"]],
    ["host", ["hostname", "platform", "arch"]],
  ] as const) {
    const before = object(baseline[group], group);
    const after = object(candidate[group], group);
    for (const field of fields) {
      if (text(before[field], `${group}.${field}`) !== text(after[field], `${group}.${field}`)) {
        throw new Error(`incomparable measurement: ${group}.${field} differs`);
      }
    }
  }
  const fixtures = list(baseline.fixtures, "fixtures");
  if (fixtures.length === 0) throw new Error("invalid measurement: empty fixtures");
  if (JSON.stringify(fixtures) !== JSON.stringify(list(candidate.fixtures, "fixtures"))) {
    throw new Error("incomparable measurement: fixtures differs");
  }
  const previous = rows(baseline.rows);
  const current = rows(candidate.rows);
  const failures: string[] = [];
  for (const raw of fixtures) {
    const name = text(object(raw, "fixture").name, "fixture.name");
    for (const path of ["reader", "reader_decode"]) {
      const key = `${name}/${path}`;
      const before = previous.get(key);
      const after = current.get(key);
      if (!before || !after) throw new Error(`invalid measurement: missing ${key}`);
      for (const field of ["sampleCount", "operationsPerSample", "warmupOperations", "measuredOperations"]) {
        if (positive(before[field], `${key}.${field}`) !== positive(after[field], `${key}.${field}`)) {
          throw new Error(`incomparable measurement: ${key}.${field} differs`);
        }
      }
      for (const metric of ["p50", "p99", "admissionApplicationSites"]) {
        const limit = positive(before[metric], `${key}.${metric}`);
        const actual = positive(after[metric], `${key}.${metric}`);
        if (metric === "admissionApplicationSites" && (!Number.isSafeInteger(limit) || !Number.isSafeInteger(actual))) {
          throw new Error(`invalid measurement: ${key}.${metric} must be an integer`);
        }
        if (actual > limit) failures.push(`${key} ${metric} regressed: ${actual} > ${limit}`);
      }
    }
  }
  return failures;
}
