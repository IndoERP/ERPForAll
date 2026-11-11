/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import { Harness, asof, thread } from "./support/wire";

const T = "a.thread";
const title = (value: string): Record<string, readonly unknown[]> => ({ [T]: [thread(1, { title: value })] });
const str = (value: string): { kind: "string"; value: string } => ({ kind: "string", value });

test("state: answer versus event", () => {
  // Rd(A) held; A knows 100..105 except 100 and 103 (in progress).
  const known = asof(100, 105, [100, 103]);

  // Wr(y) with y not in K(A): replace.
  let h = new Harness();
  h.answer(known, title("A"));
  h.frame(103, 1, asof(100, 103, [100]), title("E"));
  expect(h.table.value(T, 1, "title")).toEqual(str("E"));

  // Wr(y) with y in K(A): keep.
  h = new Harness();
  h.answer(known, title("A"));
  h.frame(102, 1, asof(100, 102, [100]), title("E"));
  expect(h.table.value(T, 1, "title")).toEqual(str("A"));

  // Wr(x) held, x = 104. Rd(A') with x in K(A'): replace.
  h = new Harness();
  h.frame(104, 1, asof(100, 104, [100, 103]), title("E"));
  h.answer(asof(100, 106), title("B"));
  expect(h.table.value(T, 1, "title")).toEqual(str("B"));

  // Wr(x) held, Rd(A') with x not in K(A') (still in progress for A'): keep.
  h = new Harness();
  h.frame(104, 1, asof(100, 104, [100, 103]), title("E"));
  h.answer(asof(100, 106, [104]), title("B"));
  expect(h.table.value(T, 1, "title")).toEqual(str("E"));
});

test("state: event versus event, same-transaction ties", () => {
  const held = (): Harness => {
    const h = new Harness();
    h.frame(105, 10, asof(100, 105), title("X"));
    return h;
  };

  // y = 106 > x = 105 and D_y knows x (the usual commit order): replace. A1: a frame's own is never below xmax,
  // so a descriptor that knows x = 105 belongs to a transaction with a higher xid.
  let h = held();
  h.frame(106, 11, asof(100, 106), title("Y"));
  expect(h.table.value(T, 1, "title")).toEqual(str("Y"));

  // y = 106 > x = 105 but x is in progress for D_y: keep. Plain xid order would replace here, so this case
  // tells the reflection predicate from the xid order.
  h = held();
  h.frame(106, 11, asof(100, 106, [105]), title("Y"));
  expect(h.table.value(T, 1, "title")).toEqual(str("X"));

  // y = 104 (a lower xid delivered later) whose descriptor does not know x: keep.
  h = held();
  h.frame(104, 11, asof(100, 104), title("Y"));
  expect(h.table.value(T, 1, "title")).toEqual(str("X"));

  // y = x with a higher event id: replace.
  h = held();
  h.frame(105, 11, asof(100, 105), title("Y"));
  expect(h.table.value(T, 1, "title")).toEqual(str("Y"));

  // y = x with a lower id, and with the same id: keep.
  h = held();
  h.frame(105, 9, asof(100, 105), title("Y"));
  expect(h.table.value(T, 1, "title")).toEqual(str("X"));
  h.frame(105, 10, asof(100, 105), title("Z"));
  expect(h.table.value(T, 1, "title")).toEqual(str("X"));
});

test("state: answer versus answer and tie-break", () => {
  const held = (): Harness => {
    const h = new Harness();
    h.answer(asof(100, 105, [100, 103]), title("A")); // finished count 103
    return h;
  };

  // A larger finished count replaces.
  let h = held();
  h.answer(asof(100, 106, [100, 103]), title("B"));
  expect(h.table.value(T, 1, "title")).toEqual(str("B"));

  // A smaller one keeps.
  h = held();
  h.answer(asof(100, 104, [100, 102]), title("B"));
  expect(h.table.value(T, 1, "title")).toEqual(str("A"));

  // An equal one with a different value (impossible under A2, test-only) keeps the held value.
  h = held();
  h.answer(asof(100, 104, [100]), title("B"));
  expect(h.table.value(T, 1, "title")).toEqual(str("A"));

  h = held();
  h.answer(asof(100, 105, [100]), title("B"));
  expect(h.table.value(T, 1, "title")).toEqual(str("B"));
});

// Found by the convergence property: a writing answer that does not mark a coordinate it wrote
// carries it as a read, and the same transaction's event must not leave it open to an older answer with a larger
// finished count. Revisions of answers and events compare against the completed set C, not against K.
test("state: an answer's own unmarked write and its event are the same writer", () => {
  const own = asof(100, 105, [], 105); // transaction 105 answers and wrote the title; finished count 105
  const newer = asof(100, 107, [105]); // a later read that does not know 105; finished count 106
  const mine = (): Harness => {
    const h = new Harness();
    h.answer(own, title("W"));
    return h;
  };

  // The answer first, then its event, then the newer read: the event upgrades the revision, so the read cannot win.
  let h = mine();
  h.frame(105, 1, asof(100, 105), title("W"));
  h.answer(newer, title("OLD"));
  expect(h.table.value(T, 1, "title")).toEqual(str("W"));

  // The event first: the same transaction's answer does not downgrade it.
  h = new Harness();
  h.frame(105, 1, asof(100, 105), title("W"));
  h.answer(own, title("W"));
  h.answer(newer, title("OLD"));
  expect(h.table.value(T, 1, "title")).toEqual(str("W"));

  // A read that completed 105 still replaces the value, as before.
  h = mine();
  h.answer(asof(100, 106), title("NEXT"));
  expect(h.table.value(T, 1, "title")).toEqual(str("NEXT"));
});

test("re-application is idempotent", () => {
  const h = new Harness();
  const request = h.table.beginRequest();
  const answer = h.decodeAnswer(
    asof(100, 104, [100, 103], 104),
    {
      [T]: [thread(1, { title: "t", messages: [1, 2], unread: 10 })],
      "b.message": [{ id: 1, text: "m" }, { id: 2, text: "n" }],
    },
    { [T]: { "1": ["title"] } },
  );
  // Transaction 103 is in progress for the answer: its counter delta and its add are not reflected.
  const bump = h.decodeFrame(103, 1, asof(100, 103, [100]), { [T]: [thread(1, { unread: { inc: 2 }, messages: { add: [3] } })] });
  // A delete (by transaction 100, which the answer does not know) while a request is outstanding leaves a tombstone.
  const remove = h.decodeFrame(100, 2, asof(100, 100), { "b.message": [{ id: 2, delete: true }] });

  const applyAll = (): void => {
    h.table.applyAnswer(answer, request);
    h.table.applyFrame(bump);
    h.table.applyFrame(remove);
  };
  applyAll();
  const once = h.table.snapshot();
  expect(h.table.counter(T, 1, "unread")).toEqual({ value: 12, saturated: false, stale: false });
  expect(h.table.members(T, 1, "messages")).toEqual([1, 3]);
  expect(h.table.hasTombstone("b.message", 2)).toBe(true);

  applyAll();
  expect(h.table.snapshot()).toEqual(once);
  applyAll();
  expect(h.table.snapshot()).toEqual(once);
});
