/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// @vitest-environment jsdom
// jsdom has no layout: row heights come through the documented `observe` seam of VariableList, driven by
// the stub below, never by faked DOM geometry.
import { expect, test, vi } from "vitest";
import * as $ from "@erpforall/elang/abi";
import { Component, VariableList, flushSync, signal, unmount } from "../src/index.js";
import type { VariableListObserve } from "../src/index.js";
import { mountApp, tpl } from "./harness.js";

type Item = { id: number };
const ids = (from: number, to: number): Item[] => Array.from({ length: to - from }, (_, i) => ({ id: from + i }));

/** Fixture options: `auto` reports heights while the row is built; otherwise `flushReports()` does. */
async function fixture(opts: {
  items?: Item[]; height?: (id: number) => number; auto?: boolean; overscan?: number;
  viewport?: number; followEnd?: boolean; estimated?: number; errors?: unknown[]; defaultObserver?: boolean;
} = {}) {
  const viewportHeight = signal(opts.viewport ?? 60);
  let keyCalls = 0;
  const items = signal(opts.items ?? ids(0, 20));
  const height = opts.height ?? (() => 20);
  const live = new Map<number, (h: number) => void>();
  const built: number[] = [];
  let disposed = 0;
  const observe: VariableListObserve = (element, report) => {
    const id = Number(element.textContent);
    live.set(id, report);
    if (opts.auto === true) report(height(id));
    return () => { disposed++; live.delete(id); };
  };
  class Host extends Component {
    static override template = "vlist.Host";
    static override components = { VariableList };
  }
  const mounted = await mountApp(Host, [tpl("vlist.Host", self => {
    const node = document.createElement("div");
    $.component(node, null, self, "VariableList", {
      items: () => items(), estimatedHeight: () => opts.estimated ?? 20, viewportHeight: () => viewportHeight(),
      overscan: () => opts.overscan ?? 0, getKey: () => (item: Item) => { keyCalls++; return `m${item.id}`; },
      followEnd: () => opts.followEnd, observe: () => (opts.defaultObserver === true ? undefined : observe),
    }, null, { default: { render(scope) {
      const row = scope as { item: () => Item };
      const span = document.createElement("span");
      span.textContent = String(row.item().id);
      built.push(row.item().id);
      return span;
    } } });
    return node;
  })], opts.errors === undefined ? {} : { errors: opts.errors });
  const viewport = mounted.root.querySelector<HTMLElement>("[data-elang-variable-list]")!;
  const scrollTo = (y: number): void => { viewport.scrollTop = y; viewport.dispatchEvent(new Event("scroll")); flushSync(); };
  const rows = (): string[] => [...mounted.root.querySelectorAll("span")].map(n => n.textContent ?? "");
  const topOf = (id: number): number => {
    const span = [...mounted.root.querySelectorAll("span")].find(n => n.textContent === String(id))!;
    return Number.parseFloat(span.parentElement!.style.top);
  };
  const flushReports = (): void => { for (const [id, report] of [...live]) report(height(id)); flushSync(); };
  return { ...mounted, viewport, items, viewportHeight, keyCalls: () => keyCalls, built, live, scrollTo, rows, topOf, flushReports, disposed: () => disposed,
    body: () => viewport.firstElementChild as HTMLElement };
}

test("window and offsets use measured heights for measured rows and the estimate for the rest", async () => {
  const f = await fixture({ height: id => (id === 1 ? 50 : 20) });
  expect(f.rows()).toEqual(["0", "1", "2"]);
  expect(f.body().style.height).toBe("400px");
  f.flushReports();
  expect(f.body().style.height).toBe("430px");
  expect(f.rows()).toEqual(["0", "1"]);
  f.scrollTo(70);
  expect(f.rows()).toEqual(["2", "3", "4"]);
  expect(f.topOf(2)).toBe(70);
  expect(f.topOf(3)).toBe(90);
});

test("prepending older rows keeps the first visible row where it was", async () => {
  const height = (id: number): number => 20 + (id % 3) * 10;
  const f = await fixture({ items: ids(100, 120), height, auto: true, overscan: 2 });
  f.scrollTo(95);
  const anchorId = f.rows().map(Number).find(id => f.topOf(id) <= 95 && 95 < f.topOf(id) + height(id))!;
  const offset = 95 - f.topOf(anchorId);
  const before = f.viewport.scrollTop;
  f.items.set([...ids(90, 100), ...f.items()]); flushSync();
  expect(f.viewport.scrollTop).not.toBe(before);
  expect(f.viewport.scrollTop - f.topOf(anchorId)).toBe(offset);
  expect(Math.min(...f.built.filter(id => id < 100))).toBeGreaterThanOrEqual(98);
});

test("a row above the anchor changing height does not move the first visible row", async () => {
  const heights = new Map<number, number>();
  const f = await fixture({ items: ids(0, 30), height: id => heights.get(id) ?? 20, auto: true, overscan: 3 });
  f.scrollTo(100);
  const anchor = 5;
  const gap = f.viewport.scrollTop - f.topOf(anchor);
  heights.set(3, 50);
  f.live.get(3)!(50); flushSync();
  expect(f.viewport.scrollTop - f.topOf(anchor)).toBe(gap);
  expect(f.viewport.scrollTop).toBe(130);
});

test("append leaves the position alone unless followEnd holds the viewport at the end", async () => {
  const plain = await fixture({ auto: true });
  plain.scrollTo(100);
  plain.items.set([...plain.items(), ...ids(20, 25)]); flushSync();
  expect(plain.viewport.scrollTop).toBe(100);

  const follow = await fixture({ auto: true, followEnd: true });
  expect(follow.viewport.scrollTop).toBe(340);
  follow.items.set([...follow.items(), ...ids(20, 25)]); flushSync();
  expect(follow.viewport.scrollTop).toBe(440);
  follow.scrollTo(50);
  follow.items.set([...follow.items(), ...ids(25, 30)]); flushSync();
  expect(follow.viewport.scrollTop).toBe(50);
});

const base = (patch: Record<string, unknown>) => ({
  items: [1], getKey: (item: unknown) => item, estimatedHeight: 20, viewportHeight: 60,
  observe: () => () => undefined,
  slots: { default: { render: () => document.createElement("span") } }, ...patch,
});

test("invalid geometry, seams and keys are precise UIErrors and nothing is clamped", async () => {
  const cases: [Record<string, unknown>, string, string][] = [
    [{ items: {} }, "value", "VariableList items must be an array"],
    [{ getKey: null }, "value", "VariableList getKey must be a function"],
    [{ estimatedHeight: 0 }, "value", "VariableList estimatedHeight must be a finite positive number"],
    [{ viewportHeight: Infinity }, "value", "VariableList viewportHeight must be a finite positive number"],
    [{ overscan: 1001 }, "value", "VariableList overscan must be an integer from 0 to 1000"],
    [{ overscan: null }, "value", "VariableList overscan must be an integer from 0 to 1000"],
    [{ followEnd: "yes" }, "value", "VariableList followEnd must be a boolean"],
    [{ observe: "no" }, "value", "VariableList observe must be a function"],
    [{ items: new Array(100_001).fill(0).map((_, i) => i), estimatedHeight: 1 }, "value", "VariableList must not have more than 100000 items"],
    [{ estimatedHeight: 16_000_001 }, "value", "VariableList total height must not exceed 16000000 CSS pixels"],
    [{ items: Array.from({ length: 10_001 }, (_, i) => i), estimatedHeight: 1, viewportHeight: 10_001, overscan: 0 }, "value", "VariableList window must not exceed 10000 rows"],
    [{ items: [1, 2], getKey: () => "same" }, "loop", "Got duplicate key in t-foreach: same"],
    [{ slots: undefined }, "template", "VariableList requires a default row slot"],
    [{ observe: (_e: HTMLElement, report: (h: number) => void) => { report(0); return () => undefined; } }, "value", "VariableList reported row height must be a finite positive number"],
  ];
  for (const [patch, code, message] of cases) {
    const errors: unknown[] = [];
    await expect(mountApp(VariableList, [], { props: base(patch), errors }), message)
      .rejects.toMatchObject({ name: "UIError", code, message });
    expect(errors.length).toBeGreaterThan(0);
  }
});

test("without an observe seam and without ResizeObserver the list refuses to guess heights", async () => {
  vi.stubGlobal("ResizeObserver", undefined);
  try {
    const errors: unknown[] = [];
    await expect(mountApp(VariableList, [], { props: base({ observe: undefined }), errors }))
      .rejects.toMatchObject({ name: "UIError", code: "value", message: "VariableList needs a ResizeObserver or an observe function" });
  } finally { vi.unstubAllGlobals(); }
});

test("observers follow row lifetime, cached heights follow the keys, unmount releases everything", async () => {
  const f = await fixture({ height: id => (id === 1 ? 50 : 20) });
  f.flushReports();
  expect(f.body().style.height).toBe("430px");
  expect(f.live.size).toBe(2);
  f.scrollTo(200);
  expect(f.disposed()).toBeGreaterThan(0);
  expect([...f.live.keys()].sort((a, b) => a - b)).toEqual(f.rows().map(Number).sort((a, b) => a - b));
  const all = f.items();
  f.items.set(all.filter(item => item.id !== 1)); flushSync();
  f.items.set(all); flushSync();
  expect(f.body().style.height).toBe("400px");
  const removeListener = vi.spyOn(f.viewport, "removeEventListener");
  unmount(f.root);
  expect(f.live.size).toBe(0);
  expect(removeListener).toHaveBeenCalledWith("scroll", expect.any(Function));
});

test("the default observer ignores a not-laid-out (height 0) report and keeps the measurement", async () => {
  const callbacks: ((entries: unknown[]) => void)[] = [];
  const reportError = vi.fn();
  vi.stubGlobal("reportError", reportError);
  vi.stubGlobal("ResizeObserver", class {
    constructor(cb: (entries: unknown[]) => void) { callbacks.push(cb); }
    observe(): void {}
    disconnect(): void {}
  });
  try {
    const errors: unknown[] = [];
    const f = await fixture({ defaultObserver: true, errors });
    expect(callbacks.length).toBe(3);
    const entry = (blockSize: number) => ({ borderBoxSize: [{ blockSize }] });
    callbacks[1]!([entry(50)]); flushSync();
    expect(f.body().style.height).toBe("430px");
    callbacks[1]!([entry(0)]); flushSync();
    expect(f.body().style.height).toBe("430px");
    expect(errors).toEqual([]);
    expect(reportError).not.toHaveBeenCalled();
  } finally { vi.unstubAllGlobals(); }
});

test("an invalid height from a custom seam reaches the owner's error channel, not the observer callback", async () => {
  // The root has no onError handler of its own here, so the error ends in the app's report channel
  // (the mount's onError, which the harness collects) and the root shows its crash panel.
  for (const bad of [Number.NaN, -5, "20"] as unknown as number[]) {
    const errors: unknown[] = [];
    const f = await fixture({ errors });
    expect(() => f.live.get(0)!(bad)).not.toThrow();
    expect(errors).toHaveLength(1);
    expect(errors[0]).toMatchObject({ name: "UIError", code: "value", message: "VariableList reported row height must be a finite positive number" });
  }
});

test("a report after the row was disposed is ignored", async () => {
  const errors: unknown[] = [];
  const f = await fixture({ errors });
  const report = f.live.get(0)!;
  unmount(f.root);
  expect(() => { report(30); report(Number.NaN); }).not.toThrow();
  expect(errors).toEqual([]);
});

test("a report for a key that left the array is not cached; same-height reports cause no relayout", async () => {
  const f = await fixture({ height: id => (id === 1 ? 50 : 20) });
  const report = f.live.get(1)!;
  const all = f.items();
  f.items.set(all.filter(item => item.id !== 1));
  report(50); // arrives before the update flushes: the key is already gone
  flushSync();
  f.items.set(all); flushSync();
  expect(f.body().style.height).toBe("400px");
  f.live.get(0)!(20); flushSync();
  const calls = f.keyCalls();
  f.live.get(0)!(20); flushSync();
  expect(f.keyCalls()).toBe(calls);
});

test("an empty list that gets populated starts at the top, or at the end with followEnd", async () => {
  const plain = await fixture({ items: [], auto: true });
  plain.items.set(ids(0, 20)); flushSync();
  expect(plain.viewport.scrollTop).toBe(0);
  const follow = await fixture({ items: [], auto: true, followEnd: true });
  follow.items.set(ids(0, 20)); flushSync();
  expect(follow.viewport.scrollTop).toBe(340);
});

test("removing every row empties the window and the list recovers when rows return", async () => {
  const f = await fixture({ auto: true });
  f.scrollTo(100);
  f.items.set([]); flushSync();
  expect(f.rows()).toEqual([]);
  expect(f.body().style.height).toBe("0px");
  expect(f.viewport.scrollTop).toBe(0);
  f.items.set(ids(0, 20)); flushSync();
  expect(f.rows()).toEqual(["0", "1", "2"]);
});

test("when the anchor row is removed the nearest surviving row takes its place", async () => {
  const f = await fixture({ items: ids(0, 30), auto: true, overscan: 3 });
  f.scrollTo(100);
  f.items.set(f.items().filter(item => item.id !== 5)); flushSync();
  expect(f.viewport.scrollTop).toBe(100);
  expect(f.topOf(6)).toBe(100);
  f.items.set(ids(0, 5)); flushSync(); // only rows above survive; position clamps to the end
  expect(f.viewport.scrollTop).toBe(40);
});

test("followEnd keeps the viewport at the end when the viewport height changes", async () => {
  const f = await fixture({ auto: true, followEnd: true });
  expect(f.viewport.scrollTop).toBe(340);
  f.viewportHeight.set(40); flushSync();
  expect(f.viewport.scrollTop).toBe(360);
  f.scrollTo(100);
  f.viewportHeight.set(60); flushSync();
  expect(f.viewport.scrollTop).toBe(100);
});

test("the list container carries a label and disables browser scroll anchoring", async () => {
  const { root } = await mountApp(VariableList, [], { props: base({ ariaLabel: "Messages" }) });
  const viewport = root.querySelector<HTMLElement>("[data-elang-variable-list]")!;
  expect(viewport.getAttribute("aria-label")).toBe("Messages");
  expect(viewport.style.getPropertyValue("overflow-anchor")).toBe("none");
  await expect(mountApp(VariableList, [], { props: base({ ariaLabel: 3 }), errors: [] }))
    .rejects.toMatchObject({ code: "value", message: "VariableList ariaLabel must be a string" });
});
