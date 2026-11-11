/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Pinned runtime costs, measured on the compiled
// js-framework-benchmark row template (fixtures/compiled/contract.Bench.js): effects and signals per row,
// effect runs per update, and DOM operations per list operation (MutationObserver records on the
// <tbody>). These are ratchets: a change that raises one fails here; a change that lowers one updates
// the pin and in the same commit.

import { expect, test } from "vitest";
import { Component, type Signal, flushSync, signal } from "../src/index.js";
import { counters } from "../src/runtime/reactive.js";
import * as Bench from "./fixtures/compiled/contract.Bench.js";
import * as Multi from "./fixtures/compiled/contract.Multi.js";
import { mountApp } from "./harness.js";

type Row = { id: number; label: Signal<string> };

let nextId = 1;
function rows(n: number): Row[] {
  const out: Row[] = [];
  for (let i = 0; i < n; i++) out.push({ id: nextId++, label: signal(`row ${nextId}`) });
  return out;
}

class BenchC extends Component {
  static override template = "contract.Bench";
  rows = signal<Row[]>([]);
  selected = signal(0);
  select(id: number): void {
    this.selected.set(id);
  }
  remove(id: number): void {
    this.rows.set(this.rows().filter((r) => r.id !== id));
  }
}

/** What one operation cost: counter deltas and DOM mutation records on the observed node. */
function measure(target: Node, op: () => void) {
  const before = { ...counters };
  const observer = new MutationObserver(() => {});
  observer.observe(target, { childList: true, subtree: true, attributes: true, characterData: true });
  op();
  flushSync();
  const records = observer.takeRecords();
  observer.disconnect();
  return {
    effectsCreated: counters.effectsCreated - before.effectsCreated,
    effectsKept: counters.effectsKept - before.effectsKept,
    effectRuns: counters.effectRuns - before.effectRuns,
    signals: counters.signals - before.signals,
    records: records.length,
    added: records.reduce((n, r) => n + r.addedNodes.length, 0),
    removed: records.reduce((n, r) => n + r.removedNodes.length, 0),
  };
}

test("pinned costs of the js-framework-benchmark operations on 1,000 rows", async () => {
  const { root, instance } = await mountApp(BenchC, [Bench]);
  const tbody = root.querySelector("tbody")!;
  const results: Record<string, ReturnType<typeof measure>> = {};

  results.create = measure(tbody, () => instance.rows.set(rows(1000)));
  expect(tbody.children).toHaveLength(1000);
  results.append = measure(tbody, () => instance.rows.set([...instance.rows(), ...rows(1000)]));
  expect(tbody.children).toHaveLength(2000);
  results.replace = measure(tbody, () => instance.rows.set(rows(1000)));
  expect(tbody.children).toHaveLength(1000);
  results.update10th = measure(tbody, () => {
    const all = instance.rows();
    for (let i = 0; i < all.length; i += 10) all[i]!.label.set(`${all[i]!.label()} !!!`);
  });
  results.select = measure(tbody, () => instance.select(instance.rows()[5]!.id));
  const swapped = () => {
    const all = instance.rows().slice();
    const t = all[1]!;
    all[1] = all[998]!;
    all[998] = t;
    instance.rows.set(all);
  };
  const second = tbody.children[1];
  results.swap = measure(tbody, swapped);
  expect(tbody.children[998]).toBe(second);
  results.removeOne = measure(tbody, () => instance.remove(instance.rows()[10]!.id));
  expect(tbody.children).toHaveLength(999);
  results.clear = measure(tbody, () => instance.rows.set([]));
  expect(tbody.children).toHaveLength(0);

  // Per row: 3 bindings, all kept (class, id text, label text; each reads the row's item signal); the two
  // event handlers are listeners, not effects; 2 signals (item, index). The new rows go in as one
  // fragment; a replace clears the old rows with one textContent write.
  expect(results.create).toEqual({ effectsCreated: 3000, effectsKept: 3000, effectRuns: 3001, signals: 2000, records: 1, added: 1000, removed: 0 });
  expect(results.append).toEqual({ effectsCreated: 3000, effectsKept: 3000, effectRuns: 3001, signals: 2000, records: 1, added: 1000, removed: 0 });
  expect(results.replace).toEqual({ effectsCreated: 3000, effectsKept: 3000, effectRuns: 3001, signals: 2000, records: 2, added: 1000, removed: 2000 });
  expect(results.update10th).toEqual({ effectsCreated: 0, effectsKept: 0, effectRuns: 100, signals: 0, records: 100, added: 0, removed: 0 });
  // Selecting re-runs every row's class binding (each reads `selected`); one class is written. A
  // keyed selector that wakes only the two affected rows is's store work.
  expect(results.select).toEqual({ effectsCreated: 0, effectsKept: 0, effectRuns: 1000, signals: 0, records: 1, added: 0, removed: 0 });
  // Two moves (udomdiff's swap), each a removal and an insertion record.
  expect(results.swap).toEqual({ effectsCreated: 0, effectsKept: 0, effectRuns: 1, signals: 0, records: 4, added: 2, removed: 2 });
  expect(results.removeOne).toEqual({ effectsCreated: 0, effectsKept: 0, effectRuns: 1, signals: 0, records: 1, added: 0, removed: 1 });
  expect(results.clear).toEqual({ effectsCreated: 0, effectsKept: 0, effectRuns: 1, signals: 0, records: 1, added: 0, removed: 999 });
});

test("a binding that reads only constants costs one run and keeps nothing", async () => {
  const before = { ...counters };
  class Const extends Component {
    static override template = "contract.Multi";
    a = () => "A";
    b = () => "B";
  }
  const { root } = await mountApp(Const, [Multi]);
  expect(root.textContent).toBe("AmidB");
  expect(counters.effectsCreated - before.effectsCreated).toBe(2);
  expect(counters.effectsKept - before.effectsKept).toBe(0);
});
