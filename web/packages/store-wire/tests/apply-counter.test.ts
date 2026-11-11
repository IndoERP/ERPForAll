/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import { Harness, asof, thread } from "./support/wire";

const T = "a.thread";
const unread = (value: number): Record<string, readonly unknown[]> => ({ [T]: [thread(1, { unread: value })] });
const bump = (delta: number): Record<string, readonly unknown[]> => ({ [T]: [thread(1, { unread: { inc: delta } })] });
const view = (h: Harness, field = "unread"): unknown => h.table.counter(T, 1, field);

test("counter: base plus non-reflected deltas, base switching", () => {
  const h = new Harness(3);

  // Deltas before any base: no value is shown.
  h.frame(101, 1, asof(100, 101), bump(1));
  h.frame(102, 2, asof(100, 102), bump(2));
  expect(view(h)).toEqual({ value: null, saturated: false, stale: false });

  // A base that reflects 101 but not 102: 10 + 2.
  h.answer(asof(100, 102), unread(10));
  expect(view(h)).toEqual({ value: 12, saturated: false, stale: false });

  // A newer comparable base that reflects 102: the log entry it reflects is dropped.
  h.answer(asof(100, 104, [], 105), unread(20));
  expect(view(h)).toEqual({ value: 20, saturated: false, stale: false });
  const counterOf = (): unknown => (h.table.snapshot() as { records: Record<string, Record<string, { fields: Record<string, unknown> }>> }).records[T]?.["1"]?.fields["unread"];
  const before = counterOf();

  // An answer that is not comparable (the base's own transaction 105 is still in progress for it): unchanged.
  h.answer(asof(100, 106, [105]), unread(99));
  expect(view(h)).toEqual({ value: 20, saturated: false, stale: false });
  expect(counterOf()).toEqual(before);

  // More deltas than the bound (3): the first three are logged, the fourth asks for a resync, nothing is dropped
  // silently and the coordinate is stale.
  for (const [xid, id] of [[110, 3], [111, 4], [112, 5]] as const) {
    expect(h.frame(xid, id, asof(100, xid), bump(1)).resync).toEqual([]);
  }
  expect(view(h)).toEqual({ value: 23, saturated: false, stale: false });
  const outcome = h.frame(113, 6, asof(100, 113), bump(1));
  expect(outcome.resync).toEqual([{ model: T, id: 1, field: "unread", reason: "log_overflow" }]);
  expect(view(h)).toEqual({ value: null, saturated: false, stale: true });

  // A later delta is not logged either and reports the resync again.
  const later = h.frame(114, 7, asof(100, 114), bump(1));
  expect(later.resync).toHaveLength(1);

  // Recovery needs an answer from a request sent after the last dropped delta.
  const old = h.table.beginRequest();
  h.frame(115, 8, asof(100, 115), bump(1)); // dropped while `old` is outstanding
  const fresh = h.table.beginRequest();
  h.answer(asof(100, 117), unread(40), old);
  expect(view(h)).toEqual({ value: null, saturated: false, stale: true });
  h.answer(asof(100, 117), unread(40), fresh);
  expect(view(h)).toEqual({ value: 40, saturated: false, stale: false });
});

test("counter: committed higher xid before an answer", () => {
  const base = 10;
  // T1 (xid 100, +1) is in progress; T2 (xid 103, +1) committed. The answer reads base + 1 with xip = [100].
  const answerAsof = asof(100, 104, [100]);
  const t1 = (h: Harness): void => void h.frame(100, 1, asof(100, 100), bump(1));
  const t2 = (h: Harness): void => void h.frame(103, 2, asof(100, 103, [100]), bump(1));

  // The answer first, then the frames in cursor order.
  let h = new Harness();
  h.answer(answerAsof, unread(base + 1));
  t1(h);
  t2(h);
  expect(view(h)).toEqual({ value: base + 2, saturated: false, stale: false });

  // The frames first, then the answer.
  h = new Harness();
  t1(h);
  t2(h);
  h.answer(answerAsof, unread(base + 1));
  expect(view(h)).toEqual({ value: base + 2, saturated: false, stale: false });
});

test("capped counters", () => {
  const cap = 5;
  const capped = (value: number): Record<string, readonly unknown[]> => ({ [T]: [thread(1, { capped: value })] });
  const cbump = (delta: number): Record<string, readonly unknown[]> => ({ [T]: [thread(1, { capped: { inc: delta } })] });

  // A base at the cap: +1 keeps it saturated, -1 cannot be shown.
  let h = new Harness();
  h.answer(asof(100, 101), capped(cap));
  expect(view(h, "capped")).toEqual({ value: cap, saturated: true, stale: false });
  expect(h.frame(201, 1, asof(100, 201), cbump(1)).resync).toEqual([]);
  expect(view(h, "capped")).toEqual({ value: cap, saturated: true, stale: false });
  const down = h.frame(202, 2, asof(100, 202), cbump(-1));
  expect(down.resync).toEqual([{ model: T, id: 1, field: "capped", reason: "saturated_decrement" }]);
  expect(view(h, "capped")).toEqual({ value: null, saturated: false, stale: true });

  // A base below the cap: exact arithmetic, saturated when it reaches or crosses the cap, exact again below.
  h = new Harness();
  h.answer(asof(100, 101), capped(3));
  h.frame(201, 1, asof(100, 201), cbump(1));
  expect(view(h, "capped")).toEqual({ value: 4, saturated: false, stale: false });
  h.frame(202, 2, asof(100, 202), cbump(1));
  expect(view(h, "capped")).toEqual({ value: cap, saturated: true, stale: false });
  h.frame(203, 3, asof(100, 203), cbump(3));
  expect(view(h, "capped")).toEqual({ value: cap, saturated: true, stale: false });
  h.frame(204, 4, asof(100, 204), cbump(-5));
  expect(view(h, "capped")).toEqual({ value: 3, saturated: false, stale: false });
});
