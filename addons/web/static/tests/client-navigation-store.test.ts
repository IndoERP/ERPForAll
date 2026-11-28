/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { NAVIGATION_KEY, NavigationStore } from "../src/client/navigation-store.js";
import type { MethodAction } from "../src/client/method-actions.js";
import type { BreadcrumbItem } from "../src/client/navigation.js";
import { clearProtectedTabState } from "../src/client/session-boundary.js";

// The tab's navigation store on its own: what it keeps, what it reads back, and what happens when the browser refuses it.

afterEach(() => { sessionStorage.clear(); vi.restoreAllMocks(); });

const crumb = (id: number): BreadcrumbItem => {
  const url = `/erp/action-20?id=${id}&view_type=form`;
  return { id: `action-20-form-${id}`, title: `Record ${id}`, route: { action: "20", id: String(id), view_type: "form" }, url, isFormView: true };
};
const related = (context: Record<string, unknown>): MethodAction => ({
  result: { kind: "current_window", name: "Companies", model: "res.company", views: [{ id: null, type: "list" }], domain: [], context: {} },
  context, origin: { action: "20", id: "5", view_type: "form" }, originLabel: "Source record"
});
const ID = "0f8fad5b-d9cb-469f-a165-70867728950e:1";

/** A storage whose reads or writes the browser refuses, as with disabled storage or a full quota. */
function refusing(read: boolean): Storage {
  const fail = (): never => { throw new DOMException("refused", read ? "SecurityError" : "QuotaExceededError"); };
  return { length: 0, clear() {}, key: () => null, removeItem() {}, getItem: read ? fail : () => null, setItem: fail };
}

test("disabled storage is reported once, and the trail still works for the page", () => {
  const report = vi.fn();
  const store = new NavigationStore(2, 32, report, () => refusing(true));
  store.saveTrail(0, [crumb(5)]);
  store.saveTrail(1, [crumb(5), crumb(6)]);
  expect(report).toHaveBeenCalledTimes(1);
  expect(store.trail(1, { action: "20", id: "6", view_type: "form" })?.map(item => item.title)).toEqual(["Record 5", "Record 6"]);
});

test("a full quota is reported once however often the store is written", () => {
  const report = vi.fn();
  const store = new NavigationStore(2, 32, report, () => refusing(false));
  for (let index = 0; index < 5; index++) store.saveTrail(index, [crumb(index + 1)]);
  expect(report).toHaveBeenCalledTimes(1);
  expect((report.mock.calls[0]![0] as DOMException).name).toBe("QuotaExceededError");
});

test("a trail keeps its 32 newest crumbs and the store its 64 newest history entries", () => {
  const store = new NavigationStore(2, 32, () => {});
  store.saveTrail(0, Array.from({ length: 40 }, (_, at) => crumb(at + 1)));
  for (let index = 1; index <= 70; index++) store.saveTrail(index, [crumb(index)]);
  const data = JSON.parse(sessionStorage.getItem(NAVIGATION_KEY)!) as { trails: [number, unknown[]][] };
  expect(data.trails).toHaveLength(64);
  expect(data.trails.map(([index]) => index)).toEqual(Array.from({ length: 64 }, (_, at) => at + 7));
  // Read back by a new page: the same bounds, and the next fresh load numbers past the kept entries.
  const again = new NavigationStore(2, 32, () => {});
  expect(again.trail(70, { action: "20", id: "70", view_type: "form" })).toHaveLength(1);
  expect(again.trail(6, { action: "20", id: "6", view_type: "form" })).toBeUndefined();
  expect(again.nextIndex()).toBe(71);

  const long = new NavigationStore(2, 32, () => {});
  long.saveTrail(80, Array.from({ length: 40 }, (_, at) => crumb(at + 1)));
  const kept = new NavigationStore(2, 32, () => {}).trail(80, { action: "20", id: "40", view_type: "form" })!;
  expect(kept).toHaveLength(32);
  expect(kept[0]!.title).toBe("Record 9");
});

test("a kept trail belongs to its own record: another record of the same action does not take it", () => {
  const store = new NavigationStore(2, 32, () => {});
  store.saveTrail(3, [crumb(5), crumb(6)]);
  expect(store.trail(3, { action: "20", id: "6", view_type: "form" })).toHaveLength(2);
  expect(store.trail(3, { action: "20", id: "5", view_type: "form" })).toBeUndefined();
  expect(store.trail(3, { action: "20" })).toBeUndefined();
});

test("a kept related list is read back only when every context value is of a kind this client writes", () => {
  new NavigationStore(2, 32, () => {}).saveAction(ID, related({ lang: "en_US", tz: null, allowed_company_ids: [1, 2], active_id: 5, active_test: false, default_rate: 1.5, group_by: ["state"] }));
  expect(new NavigationStore(2, 32, () => {}).action(ID)?.context["allowed_company_ids"]).toEqual([1, 2]);
  for (const context of [{ default_partner: { id: 3 } }, { default_lines: [[0, 0, { name: "x" }]] }, { active_id: 2 ** 60 }, { "bad key": 1 }, { default_note: "x".repeat(5000) }]) {
    sessionStorage.clear();
    new NavigationStore(2, 32, () => {}).saveAction(ID, related(context));
    expect(new NavigationStore(2, 32, () => {}).action(ID)).toBeUndefined();
  }
});

test("restores the original company of a source drilldown", () => {
  const store = new NavigationStore(2, 32, () => {});
  store.saveAction(ID, { ...related({ allowed_company_ids: [1] }), sourceCompanyId: 1 });
  expect(new NavigationStore(2, 32, () => {}).action(ID)?.sourceCompanyId).toBe(1);
});

test("after the session ends the store writes nothing back", () => {
  const store = new NavigationStore(2, 32, () => {});
  store.saveAction(ID, related({ lang: "en_US" }));
  store.saveTrail(0, [crumb(5)]);
  clearProtectedTabState();
  expect(sessionStorage.getItem(NAVIGATION_KEY)).toBeNull();
  store.removeAction(ID);
  store.saveTrail(1, [crumb(6)]);
  store.saveAction(ID, related({ lang: "en_US" }));
  expect(sessionStorage.getItem(NAVIGATION_KEY)).toBeNull();
  expect(store.trail(0, { action: "20", id: "5", view_type: "form" })).toBeUndefined();
});

test("a store past its size bound drops its oldest entries and keeps the newest", () => {
  const store = new NavigationStore(2, 32, () => {});
  const wide = (index: number): BreadcrumbItem[] => Array.from({ length: 32 }, (_, at) => ({ ...crumb(at + 1), title: `${index}-${at}-${"x".repeat(480)}` }));
  for (let index = 0; index < 64; index++) store.saveTrail(index, wide(index));
  const text = sessionStorage.getItem(NAVIGATION_KEY)!;
  expect(text.length).toBeLessThanOrEqual(1024 * 1024);
  const indexes = (JSON.parse(text) as { trails: [number, unknown[]][] }).trails.map(([index]) => index);
  expect(indexes.at(-1)).toBe(63);
  expect(indexes.length).toBeLessThan(64);
  expect(indexes).toEqual(Array.from({ length: indexes.length }, (_, at) => 64 - indexes.length + at));
});

test("a store that sheds its only related list still ends within the bound: no comma is counted for the last entry", () => {
  const MAX = 1024 * 1024;
  const stored = () => sessionStorage.getItem(NAVIGATION_KEY)!;
  const named = (count: number, size: (at: number) => number): BreadcrumbItem[] => Array.from({ length: count }, (_, at) => ({ ...crumb(at + 1), title: "t".repeat(size(at)) }));
  const store = new NavigationStore(2, 32, () => {});
  store.saveAction(ID, related({ lang: "en_US" }));
  const lone = JSON.stringify((JSON.parse(stored()) as { actions: unknown[] }).actions[0]).length;
  store.saveTrail(0, [crumb(1)]);
  let index = 1;
  // Wide trails, then narrower ones, until the store is 10 to 15 thousand units under its bound (within the 64 entries).
  while (stored().length < MAX - 30000) store.saveTrail(index++, named(32, () => 480));
  while (stored().length < MAX - 15000) store.saveTrail(index++, named(32, () => 50));
  expect(index).toBeLessThan(63);
  store.saveTrail(index, named(32, () => 10));
  const before = JSON.parse(stored()) as { trails: unknown[]; actions: unknown[] };
  expect(before.actions).toHaveLength(1);
  expect(before.trails).toHaveLength(index + 1);
  // Grow the newest trail until the store is exactly one unit more than the related list (without a comma) gives back.
  const grow = MAX + lone + 1 - stored().length;
  expect(grow).toBeGreaterThan(0);
  expect(grow).toBeLessThanOrEqual(32 * 500);
  store.saveTrail(index, named(32, at => 10 + Math.floor(grow / 32) + (at < grow % 32 ? 1 : 0)));
  const after = JSON.parse(stored()) as { trails: [number, unknown[]][]; actions: unknown[] };
  expect(stored().length).toBeLessThanOrEqual(MAX);
  expect(after.actions).toEqual([]);
  expect(after.trails.map(([at]) => at)).not.toContain(0);
  expect(after.trails.at(-1)![0]).toBe(index);
  expect(new NavigationStore(2, 32, () => {}).trail(index, { action: "20", id: "32", view_type: "form" })).toHaveLength(32);
});
