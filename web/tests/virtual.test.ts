/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// @vitest-environment jsdom
import { expect, test, vi } from "vitest";
import * as $ from "@erpforall/elang/abi";
import { Component, Virtual, effect, onError, signal, unmount } from "../src/index.js";
import { mountApp, tpl } from "./harness.js";

async function virtualFixture() {
  const items = signal(Array.from({ length: 20 }, (_, id) => ({ id })));
  let built = 0;
  const destroyed: number[] = [];
  class Host extends Component {
    static override template = "virtual.Host";
    static override components = { Virtual };
  }
  const mounted = await mountApp(Host, [tpl("virtual.Host", self => {
    const node = document.createElement("div");
    $.component(node, null, self, "Virtual", {
      items: () => items(), itemHeight: () => 20, viewportHeight: () => 60,
      overscan: () => 1, getKey: () => (item: { id: number }) => item.id,
    }, null, { default: { render(scope) {
      built++;
      const row = scope as { item: () => { id: number }; index: () => number };
      const id = row.item().id;
      effect(() => () => { destroyed.push(id); });
      const span = document.createElement("span");
      $.effect(() => { span.textContent = `${row.index()}:${row.item().id}`; });
      return span;
    } } });
    return node;
  })]);
  const viewport = mounted.root.querySelector<HTMLElement>("[data-elang-virtual]")!;
  return { ...mounted, viewport, items, destroyed, built: () => built };
}

/// (partial: visible items and scroll; spacer geometry needs browser acceptance)
test("renders visible fixed-height rows plus overscan and updates after scroll", async () => {
  const f = await virtualFixture();
  expect(f.viewport).not.toBeNull();
  expect([...f.root.querySelectorAll("span")].map(n => n.textContent)).toEqual(["0:0", "1:1", "2:2", "3:3"]);
  expect(f.viewport.style.height).toBe("60px");
  f.viewport.scrollTop = 40; f.viewport.dispatchEvent(new Event("scroll"));
  const { flushSync } = await import("../src/index.js"); flushSync();
  expect([...f.root.querySelectorAll("span")].map(n => n.textContent)).toEqual(["1:1", "2:2", "3:3", "4:4", "5:5"]);
});

/// (partial: overlapping DOM identity and reorder; component disposal covered separately)
test("retains overlapping keyed rows and updates their original indices", async () => {
  const f = await virtualFixture();
  const kept = f.root.querySelectorAll("span")[2];
  f.viewport.scrollTop = 40; f.viewport.dispatchEvent(new Event("scroll"));
  const { flushSync } = await import("../src/index.js"); flushSync();
  expect(f.root.querySelectorAll("span")[1]).toBe(kept);
  expect(f.built()).toBe(6);
  expect(f.destroyed).toEqual([0]);
  f.items.set(f.items().toReversed()); flushSync();
  expect([...f.root.querySelectorAll("span")].map(n => n.textContent)).toEqual(["1:18", "2:17", "3:16", "4:15", "5:14"]);
  expect(f.destroyed).toEqual([0, 1, 2, 3, 4, 5]);
});

/// (partial: shrink, empty and listener teardown; invalid configuration in later cases)
test("clamps after shrink, handles empty items, and removes its scroll listener", async () => {
  const f = await virtualFixture();
  const { flushSync } = await import("../src/index.js");
  f.viewport.scrollTop = 300; f.viewport.dispatchEvent(new Event("scroll")); flushSync();
  f.items.set([{ id: 99 }]); flushSync();
  expect(f.viewport.scrollTop).toBe(0);
  expect(f.root.querySelector("span")?.textContent).toBe("0:99");
  f.items.set([]); flushSync();
  expect(f.root.querySelectorAll("span")).toHaveLength(0);
  const removeListener = vi.spyOn(f.viewport, "removeEventListener");
  unmount(f.root);
  expect(removeListener).toHaveBeenCalledWith("scroll", expect.any(Function));
  f.viewport.scrollTop = 40; f.viewport.dispatchEvent(new Event("scroll")); flushSync();
  expect(f.root.textContent).toBe("");
});

for (const [patch, message] of [
  [{ items: {} }, "Virtual items must be an array"],
  [{ getKey: null }, "Virtual getKey must be a function"],
  [{ itemHeight: 0 }, "Virtual itemHeight must be a finite positive number"],
  [{ viewportHeight: Infinity }, "Virtual viewportHeight must be a finite positive number"],
  [{ overscan: null }, "Virtual overscan must be an integer from 0 to 1000"],
  [{ overscan: 1.5 }, "Virtual overscan must be an integer from 0 to 1000"],
  [{ itemHeight: 16_000_001 }, "Virtual total height must not exceed 16000000 CSS pixels"],
  [{ items: Array.from({length: 10_001}, (_, i) => i), itemHeight: 1, viewportHeight: 10_001, overscan: 0 }, "Virtual window must not exceed 10000 rows"],
] as const) {
  test(`invalid virtualizer input: ${message} ${JSON.stringify(Object.keys(patch))}`, async () => {
    const errors: unknown[] = [];
    await expect(mountApp(Virtual, [], { props: {
      items: [1], getKey: (item: unknown) => item, itemHeight: 20, viewportHeight: 60,
      slots: { default: { render: () => document.createElement("span") } }, ...patch,
    }, errors })).rejects.toMatchObject({name: "UIError", code: "value", message});
    expect(errors).toHaveLength(1);
  });
}

test("missing row slot is a precise template error", async () => {
  const errors: unknown[] = [];
  await expect(mountApp(Virtual, [], {props: {
    items: [], getKey: (item: unknown) => item, itemHeight: 20, viewportHeight: 60,
  }, errors})).rejects.toMatchObject({name: "UIError", code: "template", message: "Virtual requires a default row slot"});
  expect(errors).toHaveLength(1);
});

test("duplicate visible keys keep the keyed-list error kind and message", async () => {
  const errors: unknown[] = [];
  await expect(mountApp(Virtual, [], {props: {
    items: [1, 2], getKey: () => "same", itemHeight: 20, viewportHeight: 60,
    slots: {default: {render: () => document.createElement("span")}},
  }, errors})).rejects.toMatchObject({name: "UIError", code: "loop", message: "Got duplicate key in t-foreach: same"});
  expect(errors).toHaveLength(1);
});

test("two independent app roots keep window and disposal state isolated", async () => {
  const left = await virtualFixture(), right = await virtualFixture();
  const {flushSync} = await import("../src/index.js");
  left.viewport.scrollTop = 100; left.viewport.dispatchEvent(new Event("scroll")); flushSync();
  expect(left.root.querySelector("span")?.textContent).toBe("4:4");
  expect(right.root.querySelector("span")?.textContent).toBe("0:0");
  unmount(left.root);
  right.viewport.scrollTop = 40; right.viewport.dispatchEvent(new Event("scroll")); flushSync();
  expect(right.root.querySelector("span")?.textContent).toBe("1:1");
  expect(left.root.textContent).toBe("");
});

test("a caught duplicate-key update preserves prior row and spacer geometry and can recover", async () => {
  const items = signal([1, 2]), height = signal(20);
  const caught: unknown[] = [];
  class Recovering extends Virtual<number> {
    override setup() { onError(error => { caught.push(error); }); }
  }
  const {root} = await mountApp(Recovering, [], {props: {
    get items() {return items();}, get itemHeight() {return height();},
    viewportHeight: 60, getKey: (item: number) => item,
    slots: {default: {render(scope: object | undefined) {
      const row = scope as {item: () => number};
      const span = document.createElement("span");
      $.effect(() => {span.textContent = String(row.item());});
      return span;
    }}},
  }});
  const {flushSync} = await import("../src/index.js");
  const viewport = root.querySelector<HTMLElement>("[data-elang-virtual]")!;
  const before = viewport.innerHTML;
  items.set([1, 1]); height.set(40); flushSync();
  expect(viewport.innerHTML).toBe(before);
  expect(caught.length).toBeGreaterThan(0);
  for (const error of caught) expect(error).toMatchObject({name: "UIError", code: "loop", message: "Got duplicate key in t-foreach: 1"});
  items.set([3, 4]); flushSync();
  expect([...root.querySelectorAll("span")].map(n => n.textContent)).toEqual(["3", "4"]);
  expect(root.querySelector<HTMLElement>("[role=listitem]")?.style.height).toBe("40px");
});

test("fractional row geometry uses original offsets and one total spacer height", async () => {
  const {root} = await mountApp(Virtual, [], {props: {
    items: [0, 1, 2], getKey: (item: unknown) => item, itemHeight: 20.1, viewportHeight: 40.2, overscan: 0,
    slots: {default: {render: () => document.createElement("span")}},
  }});
  const viewport = root.querySelector<HTMLElement>("[data-elang-virtual]")!;
  expect(viewport.firstElementChild?.getAttribute("style")).toContain("height:");
  expect(Number.parseFloat((viewport.firstElementChild as HTMLElement).style.height)).toBeCloseTo(60.3);
  const rows = root.querySelectorAll<HTMLElement>("[role=listitem]");
  expect(rows).toHaveLength(2);
  expect(rows[1]!.style.position).toBe("absolute");
  expect(Number.parseFloat(rows[1]!.style.top)).toBeCloseTo(20.1);
});
