/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Ordinary functional coverage separated from costs.test.ts's milestone-end 1,000-row cost pins.
import { expect, test } from "vitest";
import { Component, type Signal, flushSync, signal, unmount } from "../src/index.js";
import * as Bench from "./fixtures/compiled/contract.Bench.js";
import * as Multi from "./fixtures/compiled/contract.Multi.js";
import { mountApp } from "./harness.js";

type Row = { id: number; label: Signal<string> };
const row = (id: number): Row => ({ id, label: signal(`row ${id}`) });

class Rows extends Component {
  static override template = "contract.Bench";
  rows = signal<Row[]>([]);
  selected = signal(0);
  select(id: number): void { this.selected.set(id); }
  remove(id: number): void { this.rows.set(this.rows().filter((item) => item.id !== id)); }
}

test("compiled rows create, append, replace, update, select, reorder, remove and clear", async () => {
  const { root, instance } = await mountApp(Rows, [Bench]);
  const tbody = root.querySelector("tbody")!;
  const labels = () => Array.from(tbody.querySelectorAll("td:nth-child(2) a"), (node) => node.textContent);
  instance.rows.set([row(1), row(2)]);
  flushSync();
  expect(labels()).toEqual(["row 1", "row 2"]);
  instance.rows.set([...instance.rows(), row(3)]);
  flushSync();
  expect(labels()).toEqual(["row 1", "row 2", "row 3"]);
  const replacement = [row(4), row(5), row(6)];
  instance.rows.set(replacement);
  flushSync();
  expect(labels()).toEqual(["row 4", "row 5", "row 6"]);
  replacement[0]!.label.set("updated");
  flushSync();
  expect(labels()).toEqual(["updated", "row 5", "row 6"]);
  (tbody.children[1]!.querySelector("td:nth-child(2) a") as HTMLElement).click();
  flushSync();
  expect(instance.selected()).toBe(5);
  expect(tbody.children[1]!.classList.contains("danger")).toBe(true);
  const first = tbody.children[0];
  instance.rows.set([replacement[2]!, replacement[1]!, replacement[0]!]);
  flushSync();
  expect(labels()).toEqual(["row 6", "row 5", "updated"]);
  expect(tbody.children[2]).toBe(first);
  (tbody.children[1]!.querySelector("td:nth-child(3) a") as HTMLElement).click();
  flushSync();
  expect(labels()).toEqual(["row 6", "updated"]);
  instance.rows.set([]);
  flushSync();
  expect(tbody.children).toHaveLength(0);
  // Detached rows must stop reacting after removal, independently of operation-cost counters.
  replacement[0]!.label.set("after removal");
  flushSync();
  expect(first!.textContent).toContain("updated");
  expect(first!.textContent).not.toContain("after removal");
});

test("constant bindings render and unmount through the compiled fragment", async () => {
  class Const extends Component {
    static override template = "contract.Multi";
    a = () => "A";
    b = () => "B";
  }
  const { root } = await mountApp(Const, [Multi]);
  expect(root.textContent).toBe("AmidB");
  unmount(root);
  expect(root.textContent).toBe("");
});
