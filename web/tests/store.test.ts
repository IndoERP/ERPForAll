/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// @vitest-environment jsdom
// Tests for store, depth rules, ColumnarRecordStore, and createSelector.

import { describe, expect, test } from "vitest";
import {
  Component,
  ColumnarRecordStore,
  UIError,
  createSelector,
  effect,
  flushSync,
  mount,
  raw,
  signal,
  store,
  toRaw,
  unmount,
} from "../src/index.js";

describe("store() and depth rules", () => {
  /// (partial: per-key read and write tracking)
  test(": store() creates a reactive proxy with per-key signal tracking", () => {
    const s = store({ count: 0, text: "initial" });
    let runs = 0;
    let observedCount = -1;

    effect(() => {
      runs++;
      observedCount = s.count;
    });
    flushSync();
    expect(runs).toBe(1);
    expect(observedCount).toBe(0);

    // Updating count notifies observers
    s.count = 42;
    flushSync();
    expect(runs).toBe(2);
    expect(observedCount).toBe(42);

    // Updating unrelated property does not re-trigger count observer
    s.text = "updated";
    flushSync();
    expect(runs).toBe(2);
    expect(s.text).toBe("updated");
  });

  test("store() depth rules: lazily wraps nested plain objects and arrays on first access", () => {
    const originalNested = { inner: 10 };
    const originalArray = [1, 2, 3];
    const s = store({
      nested: originalNested,
      items: originalArray,
    });

    let innerRuns = 0;
    let lastInner = 0;
    effect(() => {
      innerRuns++;
      lastInner = s.nested.inner;
    });
    flushSync();
    expect(innerRuns).toBe(1);
    expect(lastInner).toBe(10);

    s.nested.inner = 20;
    flushSync();
    expect(innerRuns).toBe(2);
    expect(lastInner).toBe(20);

    // Array reactivity: tracking elements and length
    let arrayLength = 0;
    let lengthRuns = 0;
    effect(() => {
      lengthRuns++;
      arrayLength = s.items.length;
    });
    flushSync();
    expect(lengthRuns).toBe(1);
    expect(arrayLength).toBe(3);

    s.items.push(4);
    flushSync();
    expect(lengthRuns).toBe(2);
    expect(arrayLength).toBe(4);
    expect(s.items[3]).toBe(4);
  });

  test("store() depth rules: wraps Map and Set with reactive operations and size signal", () => {
    const s = store({
      map: new Map<string, number>([["a", 1]]),
      set: new Set<string>(["x"]),
    });

    let mapSize = 0;
    let mapSizeRuns = 0;
    effect(() => {
      mapSizeRuns++;
      mapSize = s.map.size;
    });

    let mapAVal = 0;
    let mapARuns = 0;
    effect(() => {
      mapARuns++;
      mapAVal = s.map.get("a") ?? 0;
    });

    flushSync();
    expect(mapSize).toBe(1);
    expect(mapAVal).toBe(1);

    s.map.set("a", 2);
    flushSync();
    expect(mapARuns).toBe(2);
    expect(mapAVal).toBe(2);
    expect(mapSizeRuns).toBe(1); // Size didn't change

    s.map.set("b", 3);
    flushSync();
    expect(mapSizeRuns).toBe(2);
    expect(mapSize).toBe(2);

    let setHasY = false;
    let setYRuns = 0;
    effect(() => {
      setYRuns++;
      setHasY = s.set.has("y");
    });
    flushSync();
    expect(setHasY).toBe(false);

    s.set.add("y");
    flushSync();
    expect(setYRuns).toBe(2);
    expect(setHasY).toBe(true);
  });

  test(": raw(), toRaw(), and exemption rules for classes, Date, typed arrays, and frozen objects", () => {
    class CustomClass {
      constructor(public val: number) {}
    }
    const instance = new CustomClass(5);
    const date = new Date();
    const bytes = new Uint8Array([1, 2, 3]);
    const frozen = Object.freeze({ immutable: true });
    const markedRaw = raw({ untouched: "value" });

    const s = store({
      custom: instance,
      date,
      bytes,
      frozen,
      marked: markedRaw,
      plain: { foo: "bar" },
    });

    // Exempt types are returned as raw instances without proxying
    expect(s.custom).toBe(instance);
    expect(s.date).toBe(date);
    expect(s.bytes).toBe(bytes);
    expect(s.frozen).toBe(frozen);
    expect(s.marked).toBe(markedRaw);

    // toRaw retrieves the raw object itself, not a copy (review round 3, R13)
    expect(toRaw(s.plain)).toBe(toRaw(s).plain);
    expect(toRaw(s.plain)).not.toBe(s.plain);
    expect(toRaw(s)).not.toBe(s);

    // store() on already proxied store returns existing proxy identity
    expect(store(s)).toBe(s);
  });
});

describe("ColumnarRecordStore", () => {
  test("ColumnarRecordStore allocates signals only for read coordinates", () => {
    const records = new ColumnarRecordStore();

    // Set 1,000 records without creating any reactive signals yet
    for (let i = 1; i <= 1000; i++) {
      records.set(i, "name", `Partner ${i}`);
      records.set(i, "active", true);
    }
    expect(records.signalCount).toBe(0);

    let observedName = "";
    let runs = 0;
    effect(() => {
      runs++;
      observedName = records.get(42, "name") as string;
    });
    flushSync();

    expect(runs).toBe(1);
    expect(observedName).toBe("Partner 42");
    // Only 1 signal was created for (42, 'name')!
    expect(records.signalCount).toBe(1);

    // Modifying another record or another field does not notify or create unwanted signals
    records.set(43, "name", "Other");
    records.set(42, "active", false);
    flushSync();
    expect(runs).toBe(1);
    expect(records.signalCount).toBe(1);

    // Modifying observed coordinate triggers update
    records.set(42, "name", "Partner 42 Updated");
    flushSync();
    expect(runs).toBe(2);
    expect(observedName).toBe("Partner 42 Updated");

    // Deleting record removes its values and notifies the reader
    records.deleteRecord(42);
    expect(records.has(42, "name")).toBe(false);
    expect(records.has(42, "active")).toBe(false);
    flushSync();
    expect(runs).toBe(3);
    expect(observedName).toBeUndefined();
  });

  test("ColumnarRecordStore keeps a read coordinate's signal across deleteRecord and deleteField, so a later set() still notifies its reader (review round 3, R3)", () => {
    const records = new ColumnarRecordStore();
    // A reader of a coordinate that has no value yet: the delete writes undefined over undefined, so
    // the reader does not re-run and stays subscribed to the coordinate's signal.
    const seen: unknown[] = [];
    effect(() => {
      seen.push(records.get(7, "name"));
    });
    records.deleteRecord(7);
    flushSync();
    records.set(7, "name", "Seven");
    flushSync();
    expect(seen).toEqual([undefined, "Seven"]);

    const emails: unknown[] = [];
    effect(() => {
      emails.push(records.get(8, "email"));
    });
    records.deleteField("email");
    flushSync();
    records.set(8, "email", "b@example.com");
    flushSync();
    expect(emails).toEqual([undefined, "b@example.com"]);
    // The two coordinates remain subscribed, so deletion keeps their signals.
    expect(records.signalCount).toBe(2);
  });

  test("store() Map and Set iteration methods track size and notify when elements are added or deleted", () => {
    const mapStore = store(new Map<string, number>());
    let mapKeys: string[] = [];
    let mapRuns = 0;
    effect(() => {
      mapRuns++;
      mapKeys = [...mapStore.keys()];
    });
    flushSync();
    expect(mapRuns).toBe(1);
    expect(mapKeys).toEqual([]);

    mapStore.set("a", 1);
    flushSync();
    expect(mapRuns).toBe(2);
    expect(mapKeys).toEqual(["a"]);

    mapStore.delete("a");
    flushSync();
    expect(mapRuns).toBe(3);
    expect(mapKeys).toEqual([]);

    const setStore = store(new Set<number>());
    let setValues: number[] = [];
    let setRuns = 0;
    effect(() => {
      setRuns++;
      setValues = [...setStore.values()];
    });
    flushSync();
    expect(setRuns).toBe(1);
    expect(setValues).toEqual([]);

    setStore.add(42);
    flushSync();
    expect(setRuns).toBe(2);
    expect(setValues).toEqual([42]);
  });
});

describe("store() key tracking (review round 3)", () => {
  test(": reads, writes, nested proxying, added and deleted keys are tracked per key", () => {
    const s = store<{ count: number; nested: { inner: number }; extra?: string }>({ count: 0, nested: { inner: 1 } });
    const counts: number[] = [];
    const inners: number[] = [];
    const keys: string[] = [];
    effect(() => {
      counts.push(s.count);
    });
    effect(() => {
      inners.push(s.nested.inner);
    });
    effect(() => {
      keys.push(Object.keys(s).join(","));
    });
    flushSync();
    // A write notifies only the readers of that key.
    s.count = 1;
    flushSync();
    // A nested plain object is proxied on read; its write notifies its reader only.
    s.nested.inner = 2;
    flushSync();
    // An added key and a deleted key notify key readers, not value readers.
    s.extra = "x";
    flushSync();
    delete s.extra;
    flushSync();
    expect(counts).toEqual([0, 1]);
    expect(inners).toEqual([1, 2]);
    expect(keys).toEqual(["count,nested", "count,nested,extra", "count,nested"]);
    expect(toRaw(s.nested)).not.toBe(s.nested);
  });

  /// (partial: array truncation)
  test(": truncating an array through length notifies readers of the removed indices (R4)", () => {
    const s = store({ items: [1, 2, 3] });
    const seen: unknown[] = [];
    effect(() => {
      seen.push(s.items[2]);
    });
    s.items.length = 1;
    flushSync();
    expect(seen).toEqual([3, undefined]);
    s.items.push(5, 6);
    flushSync();
    expect(seen).toEqual([3, undefined, 6]);
    s.items.splice(0);
    flushSync();
    expect(seen).toEqual([3, undefined, 6, undefined]);
  });

  /// (partial: key changes seen by Object.keys)
  test(": Object.keys() of a store tracks keys added and deleted (R4)", () => {
    const s = store<Record<string, number>>({ a: 1 });
    const seen: string[] = [];
    effect(() => {
      seen.push(Object.keys(s).join(","));
    });
    s.b = 2;
    flushSync();
    delete s.a;
    flushSync();
    s.b = 3; // a value change does not change the keys
    flushSync();
    expect(seen).toEqual(["a", "a,b", "b"]);
  });

  /// (partial: presence of a key holding undefined)
  test(": `in` tracks a key that holds undefined being added and deleted (R22)", () => {
    const s = store<Record<string, unknown>>({});
    const seen: boolean[] = [];
    effect(() => {
      seen.push("x" in s);
    });
    s.x = undefined;
    flushSync();
    expect(Object.prototype.hasOwnProperty.call(toRaw(s), "x")).toBe(true);
    delete s.x;
    flushSync();
    expect(seen).toEqual([false, true, false]);
  });

  test("Map has() tracks a key that holds undefined being deleted (R22)", () => {
    const m = store(new Map<string, unknown>([["k", undefined]]));
    const seen: boolean[] = [];
    effect(() => {
      seen.push(m.has("k"));
    });
    m.delete("k");
    flushSync();
    m.set("k", undefined);
    flushSync();
    expect(seen).toEqual([true, false, true]);
  });

  test("Map iteration is notified when a value is replaced and yields reactive nested values (R12)", () => {
    const m = store(new Map<string, { n: number }>([["a", { n: 1 }]]));
    const seen: number[] = [];
    effect(() => {
      for (const v of m.values()) seen.push(v.n);
    });
    m.set("a", { n: 2 });
    flushSync();
    expect(seen).toEqual([1, 2]);
    // The value iteration yielded is a store: a nested write reaches the reader.
    const inner = m.get("a")!;
    inner.n = 3;
    flushSync();
    expect(seen).toEqual([1, 2, 3]);

    const entries: string[] = [];
    effect(() => {
      const parts: string[] = [];
      m.forEach((v, k) => parts.push(`${k}=${v.n}`));
      entries.push(parts.join(";"));
    });
    m.get("a")!.n = 4;
    flushSync();
    expect(entries).toEqual(["a=3", "a=4"]);
  });

  test("Set iteration is notified when a delete and an add keep the size (R12)", () => {
    const set = store(new Set<string>(["a"]));
    const seen: string[] = [];
    effect(() => {
      seen.push([...set].join(","));
    });
    set.delete("a");
    set.add("b");
    flushSync();
    expect(seen).toEqual(["a", "b"]);
  });

  test("toRaw() of a Map or Set store is the raw collection and store() never wraps a store twice (R13)", () => {
    const map = new Map<string, number>();
    const set = new Set<number>();
    const ms = store(map);
    const ss = store(set);
    expect(toRaw(ms)).toBe(map);
    expect(toRaw(ss)).toBe(set);
    expect(store(ms)).toBe(ms);
    expect(store(ss)).toBe(ss);
    expect(store(map)).toBe(ms);
  });
});

describe("createSelector", () => {
  test("createSelector provides O(1) keyed selection updates", () => {
    const selectedId = signal<number | null>(null);
    const isSelected = createSelector(selectedId);

    const effectRuns: Record<number, number> = { 1: 0, 2: 0, 3: 0 };
    const selections: Record<number, boolean> = { 1: false, 2: false, 3: false };

    // Register observers for items 1, 2, 3
    for (const id of [1, 2, 3]) {
      effect(() => {
        effectRuns[id] = (effectRuns[id] ?? 0) + 1;
        selections[id] = isSelected(id);
      });
    }
    flushSync();

    expect(effectRuns[1]).toBe(1);
    expect(effectRuns[2]).toBe(1);
    expect(effectRuns[3]).toBe(1);
    expect(selections[1]).toBe(false);
    expect(selections[2]).toBe(false);
    expect(selections[3]).toBe(false);

    // Select item 2: ONLY item 2 re-evaluates
    selectedId.set(2);
    flushSync();

    expect(effectRuns[1]).toBe(1);
    expect(effectRuns[2]).toBe(2);
    expect(effectRuns[3]).toBe(1);
    expect(selections[2]).toBe(true);

    // Switch selection from item 2 to item 3: ONLY item 2 and item 3 re-evaluate (2 updates total)
    selectedId.set(3);
    flushSync();

    expect(effectRuns[1]).toBe(1);
    expect(effectRuns[2]).toBe(3);
    expect(effectRuns[3]).toBe(2);
    expect(selections[2]).toBe(false);
    expect(selections[3]).toBe(true);
  });

  test("createSelector with a custom equals compares every subscribed key, not only identical ones (R14)", () => {
    const selected = signal<{ id: number }>({ id: 1 });
    const isSelected = createSelector(selected, (key: { id: number }, value) => key.id === value.id);
    const row1 = { id: 1 };
    const row2 = { id: 2 };
    const seen: string[] = [];
    effect(() => {
      seen.push(`${isSelected(row1)}/${isSelected(row2)}`);
    });
    // A new object with the id of row 2: equal by `equals`, not identical.
    selected.set({ id: 2 });
    flushSync();
    selected.set({ id: 1 });
    flushSync();
    expect(seen).toEqual(["true/false", "false/true", "true/false"]);
  });

  test("createSelector returns a disposer; a disposed selector refuses reads (R14)", () => {
    const selected = signal(1);
    const isSelected = createSelector(selected);
    expect(isSelected(1)).toBe(true);
    isSelected.dispose();
    selected.set(2);
    flushSync();
    let error: unknown;
    try {
      isSelected(2);
    } catch (e) {
      error = e;
    }
    expect(error).toBeInstanceOf(UIError);
    expect((error as UIError).code).toBe("usage");
    expect((error as UIError).message).toBe("createSelector: this selector was disposed; create a new one");
  });

  test("a selector created under a component is disposed with it (delta N3)", async () => {
    const selected = signal(1);
    let isSelected: ((key: number) => boolean) | undefined;
    class Owning extends Component {
      static override template = "test.Owning";
      override setup() {
        isSelected = createSelector(selected);
      }
    }
    const container = document.createElement("div");
    await mount(Owning, container, { templates: [{ name: "test.Owning", render: () => document.createElement("div") }] });
    expect(isSelected!(1)).toBe(true);
    unmount(container);
    let error: unknown;
    try {
      isSelected!(1);
    } catch (e) {
      error = e;
    }
    expect(error).toBeInstanceOf(UIError);
    expect((error as UIError).code).toBe("usage");
    expect((error as UIError).message).toBe("createSelector: this selector was disposed; create a new one");
  });
});
