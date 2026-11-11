/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import { Decoder, SnapshotDescriptor, WireError, toPayloadDoc } from "../src/index";
import type { RefusalDoc } from "../src/generated/RefusalDoc.d";
import { Harness, asof, fixtures, limitsOf, payloadCase, refusalOf, runPayloadCase, thread } from "./support/wire";

/** The refusal Rust recorded for a payload case; fails when the case is not a refusal. */
function rustRefusal(name: string): RefusalDoc {
  const outcome = payloadCase(name).outcome;
  if (outcome.outcome !== "refused") throw new Error(`fixture case ${name} is not a refusal`);
  return outcome.refusal;
}

/** The TypeScript decoder on a fixture case: the refusal, and proof that it yielded no payload. */
function tsRefusal(name: string): RefusalDoc {
  const observed = runPayloadCase(payloadCase(name));
  if (observed.outcome !== "refused") throw new Error(`case ${name} was not refused: ${observed.outcome}`);
  return observed.refusal;
}

function schemaRefusal(name: string): { rust: RefusalDoc; ts: RefusalDoc } {
  const c = fixtures.schema_cases.find((entry) => entry.name === name);
  if (c === undefined) throw new Error(`fixture schema case ${name} is missing`);
  if (c.outcome.outcome !== "refused") throw new Error(`fixture schema case ${name} is not a refusal`);
  const decoder = new Decoder(limitsOf(c.limits));
  try {
    decoder.decodeSchema(c.wire);
  } catch (error) {
    return { rust: c.outcome.refusal, ts: refusalOf(error) };
  }
  throw new Error(`schema case ${name} was not refused`);
}

/** Decode `text` as a frame or answer against the fixture schema g1 and return the refusal. */
function refusalOfText(text: string, kind: "answer" | "frame"): RefusalDoc {
  const decoder = new Decoder();
  const schemaText = fixtures.schemas["g1"];
  if (schemaText === undefined) throw new Error("fixture schema g1 is missing");
  decoder.setSchema(decoder.decodeSchema(schemaText));
  try {
    if (kind === "frame") decoder.decodeFrame(text);
    else decoder.decodeAnswer(text);
  } catch (error) {
    return refusalOf(error);
  }
  throw new Error("expected a refusal");
}

test("unknown wire major is refused first", () => {
  expect(tsRefusal("refuse_wire_major_2_before_content")).toEqual(rustRefusal("refuse_wire_major_2_before_content"));
  const schema = schemaRefusal("refuse_schema_wire_major_2");
  expect(schema.ts).toEqual(schema.rust);
  // The refusal names no content path: the invalid content behind it was never examined.
  expect(tsRefusal("refuse_wire_major_2_before_content").path).toBeNull();
});

test("schema generation change asks for the schema and bad schemas are refused", () => {
  for (const name of ["answer_needs_schema_when_none_held", "answer_needs_schema_when_other_generation_held"]) {
    expect(runPayloadCase(payloadCase(name)), name).toEqual(payloadCase(name).outcome);
  }
  // The decoder holds g0; g1 arrives; after the g1 schema is supplied the same bytes decode.
  const held = payloadCase("answer_needs_schema_when_other_generation_held");
  const decoder = new Decoder();
  const g0 = fixtures.schemas["g0"];
  const g1 = fixtures.schemas["g1"];
  if (g0 === undefined || g1 === undefined) throw new Error("fixture schemas are missing");
  decoder.setSchema(decoder.decodeSchema(g0));
  const first = decoder.decodeAnswer(held.wire);
  expect(first).toEqual({ outcome: "needs_schema", generation: "g1" });
  decoder.setSchema(decoder.decodeSchema(g1));
  const second = decoder.decodeAnswer(held.wire);
  const full = payloadCase("answer_every_kind_with_written_and_deletes");
  expect(held.wire).toBe(full.wire);
  expect(second.outcome).toBe("decoded");
  if (second.outcome === "decoded") expect({ outcome: "decoded", payload: toPayloadDoc(second.payload) }).toEqual(full.outcome);
  for (const name of [
    "refuse_schema_unknown_kind",
    "refuse_schema_many2one_without_target",
    "refuse_schema_target_not_a_model",
    "refuse_schema_cap_on_int",
    "refuse_schema_sync_on_counter",
  ]) {
    const refusal = schemaRefusal(name);
    expect(refusal.ts, name).toEqual(refusal.rust);
    expect(refusal.ts.code, name).toBe("bad_schema");
  }
});

test("unknown model is refused and nothing is applied", () => {
  const rust = rustRefusal("refuse_unknown_model");
  expect(tsRefusal("refuse_unknown_model")).toEqual(rust);
  // The whole payload yields nothing: the decoder throws, there is no partial result to apply.
  const decoder = new Decoder();
  const c = payloadCase("refuse_unknown_model");
  const schemaText = fixtures.schemas["g1"];
  if (schemaText === undefined) throw new Error("fixture schema g1 is missing");
  decoder.setSchema(decoder.decodeSchema(schemaText));
  expect(() => decoder.decodeAnswer(c.wire)).toThrow(WireError);
});

test("unknown field is refused and nothing is applied", () => {
  expect(tsRefusal("refuse_unknown_field")).toEqual(rustRefusal("refuse_unknown_field"));
});

test("unknown or disallowed operations are refused", () => {
  for (const name of [
    "refuse_unknown_x2many_operation",
    "refuse_x2many_replace_in_frame",
    "refuse_answer_only_field_in_frame",
    "refuse_counter_delta_in_answer",
  ]) {
    expect(tsRefusal(name), name).toEqual(rustRefusal(name));
  }
  // `inc` aimed at a non-counter field has no fixture: the expectation is the spec's wording for case 021,
  // `operation_not_allowed` at the field's path.
  const base = JSON.parse(payloadCase("frame_inc_delta_and_delete").wire) as {
    records: Record<string, { id: number; [k: string]: unknown }[]>;
  };
  const records = base.records["test.thread"];
  if (records === undefined || records[1] === undefined) throw new Error("fixture frame changed shape");
  records[1]["title"] = { inc: 1 };
  expect(refusalOfText(JSON.stringify(base), "frame")).toEqual({
    code: "operation_not_allowed",
    path: "records.test.thread[1].title",
    limit: null,
  });
});

test("written markers are validated and turn their coordinates into write revisions", () => {
  for (const name of [
    "refuse_written_without_own",
    "refuse_written_unknown_record",
    "refuse_written_field_not_set",
    "refuse_written_counter",
    "refuse_written_duplicate_name",
  ]) {
    expect(tsRefusal(name), name).toEqual(rustRefusal(name));
  }
  // A valid marker: the decoded form flags exactly the named fields (as Rust recorded them)...
  const full = payloadCase("answer_every_kind_with_written_and_deletes");
  expect(runPayloadCase(full)).toEqual(full.outcome);
  // ...and the table holds those coordinates as Wr(own, D) and the others as Rd(A).
  const h = new Harness();
  const a = asof(100, 104, [100, 103], 104);
  h.answer(a, { "a.thread": [thread(7, { title: "t", messages: [1], stats: "s" })] }, undefined, { "a.thread": { "7": ["title", "messages"] } });
  const snapshot = h.table.snapshot() as {
    records: Record<string, Record<string, { fields: Record<string, { rev?: string; base?: { rev: string } }> }>>;
  };
  const fields = snapshot.records["a.thread"]?.["7"]?.fields;
  expect(fields?.["title"]?.rev?.startsWith("W[104|")).toBe(true);
  expect(fields?.["messages"]?.base?.rev.startsWith("W[104|")).toBe(true);
  expect(fields?.["stats"]?.rev?.startsWith("R[")).toBe(true);
  expect(h.table.value("a.thread", 7, "title")).toEqual({ kind: "string", value: "t" });
});

test("a descriptor whose own xid is below xmax is refused (A1)", () => {
  const reason = "own is below xmax";
  const cases: { readonly name: string; readonly doc: ReturnType<typeof asof> }[] = [
    { name: "own inside the range", doc: asof(100, 105, [100, 103], 104) },
    { name: "own at xmin", doc: asof(100, 105, [], 100) },
  ];
  for (const c of cases) {
    for (const build of [() => SnapshotDescriptor.fromWire(c.doc), () => SnapshotDescriptor.create(100n, 105n, [], BigInt(c.doc.own ?? "0"))]) {
      let thrown: unknown;
      try {
        build();
      } catch (error) {
        thrown = error;
      }
      expect(thrown, c.name).toBeInstanceOf(WireError);
      const error = thrown as WireError;
      expect({ code: error.code, path: error.path, limit: error.limit }, c.name).toEqual({ code: "bad_descriptor", path: "asof.own", limit: null });
      expect(error.message, c.name).toBe(`bad_descriptor at asof.own: ${reason}`);
    }
  }
  // At xmax, and above it, own is accepted; so is no own at all.
  expect(SnapshotDescriptor.fromWire(asof(100, 105, [100], 105)).own).toBe(105n);
  expect(SnapshotDescriptor.fromWire(asof(100, 105, [100], 200)).own).toBe(200n);
  expect(SnapshotDescriptor.fromWire(asof(100, 105, [100])).own).toBeNull();
});
