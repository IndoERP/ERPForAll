/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import { Harness, asof, thread } from "./support/wire";

const T = "a.thread";
const messages = (value: unknown): Record<string, readonly unknown[]> => ({ [T]: [thread(1, { messages: value })] });
const members = (h: Harness): number[] | undefined => h.table.members(T, 1, "messages");

test("x2many membership per element", () => {
  const h = new Harness();

  // Events before any answer: each element is its own coordinate.
  h.simpleFrame(101, 1, messages({ add: [5] }));
  h.frame(103, 2, asof(100, 103, [100]), messages({ add: [6] }));
  h.frame(100, 3, asof(100, 100), messages({ add: [7] }));
  expect(members(h)).toEqual([5, 6, 7]);

  // A replace from an answer A (completed: 101, 102, 104; in progress: 100, 103). Elements written by a
  // transaction that A knows (5, by 101) take A's view; elements written by one A does not know (6 by 103,
  // 7 by 100) keep their own revision. 1 and 2 had nothing held.
  h.answer(asof(100, 105, [100, 103]), messages([1, 2]));
  expect(members(h)).toEqual([1, 2, 6, 7]);

  // Events with mixed reflection against A.
  h.frame(104, 4, asof(100, 104, [100, 103]), messages({ remove: [1] })); // 104 is in K(A): keep 1
  h.frame(100, 5, asof(100, 100), messages({ remove: [2] })); // 100 is not in K(A): remove 2
  h.frame(103, 6, asof(100, 103, [100]), messages({ add: [3] })); // 103 is not in K(A): add 3
  h.frame(101, 7, asof(100, 101), messages({ add: [4] })); // 101 is in K(A): A had no 4, keep it out
  expect(members(h)).toEqual([1, 3, 6, 7]);

  // An older replace B (completed: 101, 102 only; K(B) is inside K(A)) changes only the elements whose revision
  // it supersedes. Every held revision here comes from A or from a writer B does not know, so nothing changes,
  // including the elements B lists (2, 4, 5, 8).
  const before = h.table.snapshot();
  h.answer(asof(100, 103, [100]), messages([1, 2, 4, 5, 8]));
  expect(members(h)).toEqual([1, 3, 6, 7]);
  expect(h.table.snapshot()).toEqual(before);

  // A newer replace C (knows 103 and 104, not 100) supersedes element revisions of writers it knows: 6 and 3
  // (by 103) are replaced by C's view, 7 (by 100) is kept.
  h.answer(asof(100, 106, [100]), messages([1, 9]));
  expect(members(h)).toEqual([1, 7, 9]);
});
