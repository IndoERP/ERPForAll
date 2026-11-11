/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import {
  BusCursor,
  Decoder,
  LIMITS,
  SnapshotDescriptor,
  WireError,
  acceptClientMajor,
  compareCursors,
  limitMaximum,
  readOrder,
} from "../src/index";
import { fixtures, limitsOf, refusalOf, runPayloadCase } from "./support/wire";

function refusalOfCall(run: () => unknown): ReturnType<typeof refusalOf> {
  try {
    run();
  } catch (error) {
    return refusalOf(error);
  }
  throw new Error("expected a refusal");
}

test("decoder matches every Rust golden", () => {
  expect(fixtures.wire).toBe(1);
  expect(fixtures.payload_cases.length).toBeGreaterThan(0);
  expect(fixtures.schema_cases.length).toBeGreaterThan(0);
  expect(fixtures.descriptor_cases.length).toBeGreaterThan(0);

  // Payloads: valid ones decode to the canonical form Rust recorded, refusals give the same code, path and limit.
  for (const c of fixtures.payload_cases) {
    expect(runPayloadCase(c), `payload case ${c.name}`).toEqual(c.outcome);
  }

  // Schemas, under the limits each case lowers.
  for (const c of fixtures.schema_cases) {
    const decoder = new Decoder(limitsOf(c.limits));
    const expected = c.outcome;
    if (expected.outcome === "decoded") {
      expect(decoder.decodeSchema(c.wire), `schema case ${c.name}`).toEqual(expected.schema);
    } else {
      expect(refusalOfCall(() => decoder.decodeSchema(c.wire)), `schema case ${c.name}`).toEqual(expected.refusal);
    }
  }

  // Descriptors: canonical form, completed count and every reflection probe, or the refusal.
  for (const c of fixtures.descriptor_cases) {
    const expected = c.outcome;
    if (expected.outcome === "valid") {
      const d = SnapshotDescriptor.fromWire(c.wire);
      expect(d.toDoc(), `descriptor case ${c.name}`).toEqual(expected.asof);
      expect(d.completedCount().toString(), `descriptor case ${c.name} count`).toBe(expected.completed_count);
      expect(
        expected.probes.map((p) => ({ xid: p.xid, reflects: d.reflects(BigInt(p.xid)) })),
        `descriptor case ${c.name} probes`,
      ).toEqual(expected.probes);
    } else {
      expect(refusalOfCall(() => SnapshotDescriptor.fromWire(c.wire)), `descriptor case ${c.name}`).toEqual(expected.refusal);
    }
  }

  // Cursors, and their numeric order (never string order).
  for (const c of fixtures.cursor_cases) {
    const expected = c.outcome;
    if (expected.outcome === "valid") {
      expect(BusCursor.fromWire(c.wire).toDoc(), `cursor case ${c.name}`).toEqual(expected.cursor);
    } else {
      expect(refusalOfCall(() => BusCursor.fromWire(c.wire)), `cursor case ${c.name}`).toEqual(expected.refusal);
    }
  }
  const sorted = fixtures.cursor_order.shuffled.map((d) => BusCursor.fromWire(d)).sort(compareCursors);
  expect(sorted.map((c) => c.toDoc())).toEqual(fixtures.cursor_order.sorted);

  // Reads are ordered by finished count; ties keep input order.
  const order = fixtures.read_order;
  const descriptors = order.descriptors.map((d) => SnapshotDescriptor.fromWire(d));
  expect(descriptors.map((d) => d.completedCount().toString())).toEqual(order.counts);
  expect(readOrder(descriptors)).toEqual(order.ascending);

  // Client majors.
  for (const c of fixtures.client_majors) {
    if (c.accepted !== null) {
      expect(acceptClientMajor(c.client), `client major ${c.client}`).toBe(c.accepted);
    } else {
      const error = (() => {
        try {
          acceptClientMajor(c.client);
        } catch (e) {
          return e;
        }
        throw new Error("expected a refusal");
      })();
      expect(error).toBeInstanceOf(WireError);
      expect(refusalOf(error), `client major ${c.client}`).toEqual(c.refusal);
    }
  }

  // The compiled-in limit maxima.
  expect(LIMITS.map((limit) => ({ limit, value: limitMaximum(limit) }))).toEqual(fixtures.limits);
});
