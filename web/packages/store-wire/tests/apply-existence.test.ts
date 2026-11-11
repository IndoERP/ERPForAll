/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import { Harness, asof, thread } from "./support/wire";

const T = "a.thread";
const M = "b.message";

test("deletes apply after all upserts of a payload", () => {
  const h = new Harness();
  // The thread holds messages 5 and 6; message 5 is held too.
  h.answer(asof(100, 105), { [T]: [thread(1, { messages: [5, 6] })], [M]: [{ id: 5, text: "x" }, { id: 6, text: "y" }] });
  expect(h.table.hasRecord(M, 5)).toBe(true);

  // One frame: message 5 is deleted and, earlier in model order, a thread adds 5 (again) and 7. The document
  // lists the delete model first on purpose.
  const text = JSON.stringify({
    wire: 1,
    schema: "t1",
    cursor: { xid: "110", id: "1" },
    asof: asof(100, 110, [], 110),
    records: { [M]: [{ id: 5, delete: true }], [T]: [thread(1, { messages: { add: [5, 7] } })] },
  });
  const out = h.decoder.decodeFrame(text);
  if (out.outcome !== "decoded") throw new Error("needs schema");
  // Decoded canonical order: every upsert is listed apart from, and applied before, every delete.
  expect(out.payload.upserts.map((r) => [r.model, r.id])).toEqual([[T, 1]]);
  expect(out.payload.deletes).toEqual([{ model: M, id: 5 }]);

  h.table.applyFrame(out.payload);
  expect(h.table.hasRecord(M, 5)).toBe(false);
  expect(h.table.hasRecord(M, 6)).toBe(true);
  // 5 is in no held x2many of the model; 7 (not deleted) was added.
  expect(h.table.members(T, 1, "messages")).toEqual([6, 7]);
});

test("stale answer cannot resurrect; tombstone release", () => {
  const h = new Harness();
  h.answer(asof(100, 105), { [T]: [thread(1, { messages: [5] })], [M]: [{ id: 5, text: "old" }] });
  expect(h.table.hasRecord(M, 5)).toBe(true);

  // A request is outstanding when the delete event arrives: the delete leaves a tombstone.
  const early = h.table.beginRequest();
  h.frame(105, 1, asof(100, 105, [100], 105), { [M]: [{ id: 5, delete: true }] });
  expect(h.table.hasRecord(M, 5)).toBe(false);
  expect(h.table.hasTombstone(M, 5)).toBe(true);
  expect(h.table.members(T, 1, "messages")).toEqual([]);

  // A request sent after the delete was delivered.
  const late = h.table.beginRequest();

  // The stale answer (it does not know transaction 105) carries the record and still lists it: not brought back.
  h.answer(asof(100, 106, [105]), { [T]: [thread(1, { messages: [5] })], [M]: [{ id: 5, text: "stale" }] }, early);
  expect(h.table.hasRecord(M, 5)).toBe(false);
  expect(h.table.value(M, 5, "text")).toBeUndefined();
  expect(h.table.members(T, 1, "messages")).toEqual([]);

  // The later request settles first: the tombstone waits for the request outstanding at its creation.
  h.table.settleRequest(late);
  expect(h.table.hasTombstone(M, 5)).toBe(true);
  h.table.settleRequest(early);
  expect(h.table.hasTombstone(M, 5)).toBe(false);
  expect(h.table.hasRecord(M, 5)).toBe(false);

  // Nothing outstanding when a delete arrives: nothing to wait for, no tombstone is kept.
  h.answer(asof(100, 106), { [M]: [{ id: 6, text: "z" }] });
  h.frame(106, 2, asof(100, 106, [], 106), { [M]: [{ id: 6, delete: true }] });
  expect(h.table.hasRecord(M, 6)).toBe(false);
  expect(h.table.hasTombstone(M, 6)).toBe(false);
});
