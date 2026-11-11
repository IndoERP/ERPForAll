/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import { Decoder, LIMITS, WireError, WireLimits, limitMaximum } from "../src/index";
import type { LimitDoc } from "../src/generated/LimitDoc.d";
import type { RefusalDoc } from "../src/generated/RefusalDoc.d";
import { fixtures, refusalOf, runPayloadCase } from "./support/wire";

function refusal(run: () => unknown): RefusalDoc {
  try {
    run();
  } catch (error) {
    return refusalOf(error);
  }
  throw new Error("expected a refusal");
}

const SCHEMA = (models: Record<string, unknown>, token = "g1"): string => JSON.stringify({ wire: 1, schema: token, models });
const ASOF = { xmin: "100", xmax: "105", xip: ["100"], own: null };

/** An input that exceeds `limit` at the default (maximum) value, built from nothing but the limit itself. */
function overMaximum(limit: LimitDoc): { run: (decoder: Decoder) => unknown; message: string } {
  const max = limitMaximum(limit);
  const payload = (records: unknown, asof: unknown = ASOF, token = "g1"): string =>
    JSON.stringify({ wire: 1, schema: token, asof, records });
  const withSchema = (decoder: Decoder, models: Record<string, unknown>): void => {
    decoder.setSchema(decoder.decodeSchema(SCHEMA(models)));
  };
  switch (limit) {
    case "bytes":
      return { message: "bytes", run: (d) => d.decodeAnswer(" ".repeat(max + 1)) };
    case "depth":
      return { message: "depth", run: (d) => d.decodeAnswer("[".repeat(max + 1) + "]".repeat(max + 1)) };
    case "models":
      return {
        message: "models",
        run: (d) => {
          const records: Record<string, unknown[]> = {};
          for (let i = 0; i <= max; i += 1) records[`m${i}`] = [];
          withSchema(d, { m0: { fields: {} } });
          return d.decodeAnswer(payload(records));
        },
      };
    case "records":
      return {
        message: "records",
        run: (d) => {
          withSchema(d, { m: { fields: {} } });
          return d.decodeAnswer(payload({ m: Array.from({ length: max + 1 }, (_, i) => ({ id: i + 1 })) }));
        },
      };
    case "ids_per_list":
      return {
        message: "ids_per_list",
        run: (d) => {
          withSchema(d, { m: { fields: { l: { kind: "x2many", target: "m" } } } });
          return d.decodeAnswer(payload({ m: [{ id: 1, l: Array.from({ length: max + 1 }, (_, i) => i + 1) }] }));
        },
      };
    case "xip":
      return {
        message: "xip",
        run: (d) => {
          withSchema(d, { m: { fields: {} } });
          const xip = Array.from({ length: max + 1 }, (_, i) => String(i + 1));
          return d.decodeAnswer(payload({}, { xmin: "1", xmax: String(max + 2), xip, own: null }));
        },
      };
    case "string_bytes":
      return {
        message: "string_bytes",
        run: (d) => {
          withSchema(d, { m: { fields: { s: { kind: "string" } } } });
          return d.decodeAnswer(payload({ m: [{ id: 1, s: "x".repeat(max + 1) }] }));
        },
      };
    case "schema_token_bytes":
      return { message: "schema_token_bytes", run: (d) => d.decodeSchema(SCHEMA({}, "t".repeat(max + 1))) };
    case "schema_fields":
      return {
        message: "schema_fields",
        run: (d) => {
          const fields: Record<string, unknown> = {};
          for (let i = 0; i <= max; i += 1) fields[`f${i}`] = { kind: "bool" };
          return d.decodeSchema(SCHEMA({ m: { fields } }));
        },
      };
  }
}

test("every limit refuses with its name", () => {
  // The maxima are the ones Rust compiled in.
  expect(LIMITS.map((limit) => ({ limit, value: limitMaximum(limit) }))).toEqual(fixtures.limits);
  expect(LIMITS).toHaveLength(9);

  // Every limit refuses an input over its maximum, naming itself, and refuses nothing else on the way.
  for (const limit of LIMITS) {
    const over = overMaximum(limit);
    const seen = refusal(() => over.run(new Decoder()));
    expect(seen.code, limit).toBe("limit_exceeded");
    expect(seen.limit, limit).toBe(limit);
  }

  // The lowered-limit goldens, one per limit, refuse with the code, path and limit name Rust recorded.
  const lowered = fixtures.payload_cases.filter((c) => c.name.startsWith("limit_"));
  expect(lowered.map((c) => c.limits.map((l) => l.limit)).flat().sort()).toEqual(
    LIMITS.filter((limit) => limit !== "schema_fields").slice().sort(),
  );
  for (const c of lowered) expect(runPayloadCase(c), c.name).toEqual(c.outcome);

  // A host may lower a limit, never raise it above the maximum, and never set zero.
  for (const limit of LIMITS) {
    const max = limitMaximum(limit);
    expect(WireLimits.defaults().lowered(limit, max).get(limit)).toBe(max);
    expect(WireLimits.defaults().lowered(limit, 1).get(limit)).toBe(1);
    for (const value of [0, max + 1, -1, 1.5]) {
      let thrown: unknown;
      try {
        WireLimits.defaults().lowered(limit, value);
      } catch (error) {
        thrown = error;
      }
      expect(thrown, `${limit}=${value}`).toBeInstanceOf(WireError);
      expect(refusalOf(thrown), `${limit}=${value}`).toEqual({ code: "bad_limit", path: null, limit: null });
    }
  }
});
