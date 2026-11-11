/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, it } from "vitest";
import { admissionFixtures } from "../bench/fixtures";
import { decodeAst, inspectParsed, inspectValidated, parseBoundedJson } from "../src/index";

// These are not physical allocation or peak-heap bounds; no guessed slack is added.
const measured = {
  scalar_i64: { bytes: 91, reader: 28, decoder: 27, logicalBytes: 407 },
  wide_list_256: { bytes: 15188, reader: 3608, decoder: 3093, logicalBytes: 73642 },
  unicode_string: { bytes: 4171, reader: 28, decoder: 25, logicalBytes: 4487 },
};

for (const fixture of admissionFixtures) {
  it(`does not regress measured admission allocation sites: ${fixture.name}`, () => {
    const raw = parseBoundedJson(fixture.text);
    const ast = decodeAst(raw);
    const reader = inspectParsed(raw);
    const decoder = inspectValidated(ast);
    const limit = measured[fixture.name];
    expect(reader.bytes).toBe(limit.bytes);
    expect(reader.applicationAllocations).toBeLessThanOrEqual(limit.reader);
    expect(decoder.applicationAllocations).toBeLessThanOrEqual(limit.decoder);
    expect(reader.logicalAllocationBytes).toBeLessThanOrEqual(limit.logicalBytes);
    expect(reader.applicationAllocations + decoder.applicationAllocations)
      .toBeLessThanOrEqual(8 + 6 * reader.tokens);
  });
}

it("bounds application allocation sites for long plain and escaped string tokens", () => {
  const plain = parseBoundedJson(JSON.stringify({ value: "é😀".repeat(8192) }));
  const escaped = parseBoundedJson('{"value":"' + "\\u00e9\\ud83d\\ude00".repeat(8192) + '"}');
  expect(escaped).toEqual(plain);
  for (const value of [plain, escaped]) {
    const counters = inspectParsed(value);
    expect(counters.applicationAllocations).toBeLessThanOrEqual(8 + 6 * counters.tokens);
  }
});
