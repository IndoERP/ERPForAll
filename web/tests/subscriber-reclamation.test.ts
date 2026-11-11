/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import { ColumnarRecordStore, computed, createSelector, effect, flushSync, signal } from "../src/index.js";

test("untracked columnar reads do not retain signals", () => {
  const rows = new ColumnarRecordStore();
  rows.set(1, "name", "one");
  expect(rows.get(1, "name")).toBe("one");
  expect(rows.signalCount).toBe(0);
});

test("coordinates stay subscribed until the last reader stops, then can be recreated", () => {
  const rows = new ColumnarRecordStore();
  rows.set(1, "name", "one");
  let observed: unknown;
  const stopA = effect(() => { observed = rows.get(1, "name"); });
  const stopB = effect(() => { rows.get(1, "name"); });
  expect(rows.signalCount).toBe(1);
  stopA(); expect(rows.signalCount).toBe(1);
  stopB(); expect(rows.signalCount).toBe(0);
  const stopC = effect(() => { observed = rows.get(1, "name"); });
  rows.set(1, "name", "updated"); flushSync();
  expect(observed).toBe("updated");
  stopC(); expect(rows.signalCount).toBe(0);
});

test("dynamic computed dependencies release coordinates and cascade on disposal", () => {
  const rows = new ColumnarRecordStore();
  rows.set(1, "name", "one"); rows.set(2, "name", "two");
  const id = signal(1);
  const name = computed(() => rows.get(id(), "name"));
  let observed: unknown;
  const stop = effect(() => { observed = name(); });
  expect(rows.signalCount).toBe(1);
  id.set(2); flushSync();
  expect(observed).toBe("two"); expect(rows.signalCount).toBe(1);
  stop(); expect(rows.signalCount).toBe(0);
});

test("subscribed deletion keeps invalidation alive until the reader stops", () => {
  const rows = new ColumnarRecordStore();
  rows.set(1, "name", "one");
  let observed: unknown;
  const stop = effect(() => { observed = rows.get(1, "name"); });
  rows.deleteRecord(1); flushSync(); expect(observed).toBeUndefined();
  rows.set(1, "name", "new"); flushSync(); expect(observed).toBe("new");
  stop(); expect(rows.signalCount).toBe(0);
});

test("an untracked computed retains its cache dependency so later writes remain fresh", () => {
  const rows = new ColumnarRecordStore(); rows.set(1, "name", "one");
  const name = computed(() => rows.get(1, "name"));
  expect(name()).toBe("one"); expect(rows.signalCount).toBe(1);
  rows.set(1, "name", "two"); expect(name()).toBe("two");
});

test("selector dependencies can stop and subscribe again without stale values", () => {
  const selected = signal(1);
  const key = signal(1);
  const choose = createSelector(selected);
  let observed: boolean | undefined;
  const stop = effect(() => { observed = choose(key()); });
  expect(observed).toBe(true);
  key.set(2); flushSync(); expect(observed).toBe(false);
  selected.set(2); flushSync(); expect(observed).toBe(true);
  key.set(1); flushSync(); expect(observed).toBe(false);
  selected.set(1); flushSync(); expect(observed).toBe(true);
  stop(); choose.dispose();
});
