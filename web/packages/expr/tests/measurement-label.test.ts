/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, it } from "vitest";
import { admissionMetadata } from "../../../scripts/measure-expr-worker";

it("labels admission counters as fixture-level combined work on every timing path", () => {
  const measurement = (globalThis as unknown as {
    exprMeasurement: { paths: string[] };
  }).exprMeasurement;
  for (const path of measurement.paths) {
    const row = admissionMetadata(0, path);
    expect(row.admissionCounterScope).toBe("fixture-level reader+decode, independent of timed path");
    expect(row.admissionApplicationSites).toBe(
      row.readerCounters.applicationAllocations + row.decoderCounters.applicationAllocations,
    );
  }
});
