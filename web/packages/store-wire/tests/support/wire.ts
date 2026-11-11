/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Shared helpers for the store-wire suites: the Rust-written golden fixtures and a small harness that builds
// payload text, decodes it with the strict decoder and applies it to a reference table.
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import {
  Decoder,
  type DecodedPayload,
  StoreTable,
  WireError,
  WireLimits,
  toPayloadDoc,
} from "../../src/index";
import type { AsofDoc } from "../../src/generated/AsofDoc.d";
import type { FixtureFileDoc } from "../../src/generated/FixtureFileDoc.d";
import type { LimitValueDoc } from "../../src/generated/LimitValueDoc.d";
import type { PayloadCaseDoc } from "../../src/generated/PayloadCaseDoc.d";
import type { PayloadDoc } from "../../src/generated/PayloadDoc.d";
import type { RefusalDoc } from "../../src/generated/RefusalDoc.d";

/** The golden fixtures written by the Rust `store_wire_fixtures` example. */
export const fixtures: FixtureFileDoc = JSON.parse(
  readFileSync(resolve(process.cwd(), "packages/store-wire/tests/generated-fixtures.json"), "utf8"),
) as FixtureFileDoc;

/** A decode result in the shape the fixtures record. */
export type Observed =
  | { readonly outcome: "decoded"; readonly payload: PayloadDoc }
  | { readonly outcome: "needs_schema"; readonly generation: string }
  | { readonly outcome: "refused"; readonly refusal: RefusalDoc };

/** The limits of a fixture case: the defaults with each listed limit lowered. */
export function limitsOf(list: readonly LimitValueDoc[]): WireLimits {
  return list.reduce((limits, entry) => limits.lowered(entry.limit, entry.value), WireLimits.defaults());
}

/** Turn a thrown refusal into the fixture form; anything else is a bug and is rethrown. */
export function refusalOf(error: unknown): RefusalDoc {
  if (!(error instanceof WireError)) throw error;
  return { code: error.code, path: error.path, limit: error.limit };
}

/** Decode one fixture case with the TypeScript decoder. */
export function runPayloadCase(c: PayloadCaseDoc): Observed {
  const limits = limitsOf(c.limits);
  const decoder = new Decoder(limits);
  try {
    if (c.schema !== null) {
      const text = fixtures.schemas[c.schema];
      if (text === undefined) throw new Error(`fixture schema ${c.schema} is missing`);
      // The schema is read under the default limits; the case lowers limits for the payload only.
      decoder.setSchema(new Decoder().decodeSchema(text));
    }
    const out = c.kind === "answer" ? decoder.decodeAnswer(c.wire) : decoder.decodeFrame(c.wire);
    return out.outcome === "decoded"
      ? { outcome: "decoded", payload: toPayloadDoc(out.payload) }
      : { outcome: "needs_schema", generation: out.generation };
  } catch (error) {
    return { outcome: "refused", refusal: refusalOf(error) };
  }
}

/** The fixture payload case with that name. */
export function payloadCase(name: string): PayloadCaseDoc {
  const found = fixtures.payload_cases.find((c) => c.name === name);
  if (found === undefined) throw new Error(`fixture payload case ${name} is missing`);
  return found;
}

/** An `asof` object with decimal-string xids. */
export function asof(xmin: number, xmax: number, xip: readonly number[] = [], own: number | null = null): AsofDoc {
  return { xmin: String(xmin), xmax: String(xmax), xip: xip.map(String), own: own === null ? null : String(own) };
}

type Records = Record<string, readonly unknown[]>;

/** A schema with every shape the apply suites need. Models sort so that `a.thread` precedes `b.message`. */
export const APPLY_SCHEMA = JSON.stringify({
  wire: 1,
  schema: "t1",
  models: {
    "a.thread": {
      fields: {
        title: { kind: "string" },
        messages: { kind: "x2many", target: "b.message" },
        unread: { kind: "counter" },
        capped: { kind: "counter", cap: 5 },
        stats: { kind: "string", sync: "answer_only" },
      },
    },
    "b.message": { fields: { text: { kind: "string" } } },
  },
});

/** Builds, decodes and applies payloads for the apply suites. */
export class Harness {
  readonly decoder = new Decoder();
  readonly table: StoreTable;

  constructor(logBound?: number) {
    this.decoder.setSchema(this.decoder.decodeSchema(APPLY_SCHEMA));
    this.table = new StoreTable(this.decoder.decodeSchema(APPLY_SCHEMA), logBound);
  }

  /** Decode an answer built from `records` (and an optional written marker). */
  decodeAnswer(a: AsofDoc, records: Records, written?: unknown): DecodedPayload {
    const text = JSON.stringify({ wire: 1, schema: "t1", asof: a, records, ...(written === undefined ? {} : { written }) });
    return this.#decoded(this.decoder.decodeAnswer(text));
  }

  /** Decode a frame of transaction `xid` with event id `id`; the descriptor's own is `xid`. */
  decodeFrame(xid: number, id: number, a: AsofDoc, records: Records): DecodedPayload {
    // A1: a frame's descriptor is its transaction's snapshot with own = xid, and own is never below xmax.
    if (BigInt(a.xmax) > BigInt(xid)) throw new Error(`test frame of xid ${xid} needs xmax <= ${xid}, got ${a.xmax}`);
    const text = JSON.stringify({
      wire: 1,
      schema: "t1",
      cursor: { xid: String(xid), id: String(id) },
      asof: { ...a, own: String(xid) },
      records,
    });
    return this.#decoded(this.decoder.decodeFrame(text));
  }

  /** Decode and apply an answer. */
  answer(a: AsofDoc, records: Records, request?: number, written?: unknown): ReturnType<StoreTable["applyAnswer"]> {
    return this.table.applyAnswer(this.decodeAnswer(a, records, written), request);
  }

  /** Decode and apply a frame. */
  frame(xid: number, id: number, a: AsofDoc, records: Records): ReturnType<StoreTable["applyFrame"]> {
    return this.table.applyFrame(this.decodeFrame(xid, id, a, records));
  }

  /** A frame of transaction `xid` whose descriptor is `xmin = 1`, `xmax = xid`, nothing in progress. */
  simpleFrame(xid: number, id: number, records: Records): ReturnType<StoreTable["applyFrame"]> {
    return this.frame(xid, id, asof(1, xid), records);
  }

  #decoded(out: ReturnType<Decoder["decodeAnswer"]>): DecodedPayload {
    if (out.outcome !== "decoded") throw new Error(`payload needs schema ${out.generation}`);
    return out.payload;
  }
}

/** A thread record entry with one field. */
export function thread(id: number, fields: Record<string, unknown>): Record<string, unknown> {
  return { id, ...fields };
}
