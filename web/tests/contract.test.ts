/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The ABI contract: modules the compiler (merged)
// produced for tests/fixtures/compiled/contract.xml, imported as they are and run through the runtime.

import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { expect, test } from "vitest";
import { Component, type Signal, flushSync, markup, signal } from "../src/index.js";
import * as Bench from "./fixtures/compiled/contract.Bench.js";
import * as Card from "./fixtures/compiled/contract.Card.js";
import * as Child from "./fixtures/compiled/contract.Child.js";
import * as Multi from "./fixtures/compiled/contract.Multi.js";
import * as Root from "./fixtures/compiled/contract.Root.js";
import * as Rows from "./fixtures/compiled/contract.Rows.js";
import { mountApp } from "./harness.js";

const templates = [Root, Child, Card, Rows, Multi, Bench];
const fixtures = join(dirname(fileURLToPath(import.meta.url)), "fixtures", "compiled");

test("the compiled fixtures are the compiler's output, unedited (hashes in PROVENANCE.json)", () => {
  const provenance = JSON.parse(readFileSync(join(fixtures, "PROVENANCE.json"), "utf8")) as {
    compiler_commit: string;
    source: Record<string, string>;
    modules: Record<string, string>;
  };
  expect(provenance.compiler_commit).toMatch(/^[0-9a-f]{40}$/);
  const sha = (f: string) => createHash("sha256").update(readFileSync(join(fixtures, f))).digest("hex");
  for (const [file, hash] of Object.entries({ ...provenance.source, ...provenance.modules })) {
    expect(sha(file), file).toBe(hash);
  }
  expect(Object.keys(provenance.modules).sort()).toEqual(templates.map((t) => `${t.name}.js`).sort());
});

type RowData = { id: number; label: string };

class ChildC extends Component<{ label: string; onPing: (v: string) => void }> {
  static override template = "contract.Child";
}
class CardC extends Component {
  static override template = "contract.Card";
}
class RowsC extends Component<{ items: RowData[] }> {
  static override template = "contract.Rows";
}
class RootC extends Component {
  static override template = "contract.Root";
  static override components = { Child: ChildC, Card: CardC, Rows: RowsC };
  title!: Signal<string>;
  active!: Signal<boolean>;
  mode!: Signal<string>;
  count!: Signal<number>;
  rows!: Signal<RowData[]>;
  html!: Signal<unknown>;
  pings: string[] = [];
  named = 0;
  override setup(): void {
    this.title = signal("Hello");
    this.active = signal(false);
    this.mode = signal("a");
    this.count = signal(0);
    this.rows = signal([
      { id: 1, label: "one" },
      { id: 2, label: "two" },
    ]);
    this.html = signal<unknown>(undefined);
  }
  inc(): void {
    this.count.set(this.count() + 1);
  }
  onNamed(): void {
    this.named++;
  }
  ping(v: string): void {
    this.pings.push(v);
  }
}

test("compiled templates render and update text, attributes, classes, props, t-if chains and events", async () => {
  const { root, instance } = await mountApp(RootC, templates);
  const div = root.querySelector(".o_root")!;
  expect(div.getAttribute("title")).toBe("Hello");
  expect(div.classList.contains("o_active")).toBe(false);
  expect(root.querySelector("h1")!.textContent).toBe("Hello");
  expect(root.querySelector(".mode")!.textContent).toBe("mode a");
  expect((root.querySelector(".field") as HTMLInputElement).value).toBe("Hello");
  instance.title.set("Bye");
  instance.active.set(true);
  instance.mode.set("b");
  flushSync();
  expect(div.getAttribute("title")).toBe("Bye");
  expect(div.classList.contains("o_active")).toBe(true);
  expect(div.classList.contains("o_root")).toBe(true);
  expect(root.querySelector("h1")!.textContent).toBe("Bye");
  expect(root.querySelector(".mode")!.getAttribute("data-mode")).toBe("m-b");
  expect(root.querySelector(".child")!.textContent).toBe("Byeping");
  expect(root.querySelector("header")!.textContent).toBe("Header Bye");
  instance.mode.set("z");
  flushSync();
  expect(root.querySelector(".mode")!.textContent).toBe("other");
  (root.querySelector(".inc") as HTMLButtonElement).click();
  (root.querySelector(".inc") as HTMLButtonElement).click();
  flushSync();
  expect(root.querySelector(".count")!.textContent).toBe("2");
  (root.querySelector(".named") as HTMLButtonElement).click();
  expect(instance.named).toBe(1);
  (root.querySelector(".ping") as HTMLButtonElement).click();
  expect(instance.pings).toEqual(["pong"]);
});

test("compiled keyed t-foreach keeps rows by key through reorders, and slots render with scope", async () => {
  const { root, instance } = await mountApp(RootC, templates);
  const items = () => [...root.querySelectorAll("ul > li")].map((li) => `${li.getAttribute("data-id")}=${li.textContent}`);
  expect(items()).toEqual(["1=0:one", "2=1:two"]);
  expect([...root.querySelectorAll("ol i")].map((i) => i.textContent)).toEqual(["one", "two"]);
  const li2 = root.querySelector('ul > li[data-id="2"]');
  instance.rows.set([
    { id: 2, label: "TWO" },
    { id: 3, label: "three" },
    { id: 1, label: "one" },
  ]);
  flushSync();
  expect(items()).toEqual(["2=0:TWO", "3=1:three", "1=2:one"]);
  expect(root.querySelector('ul > li[data-id="2"]')).toBe(li2);
  expect([...root.querySelectorAll("ol i")].map((i) => i.textContent)).toEqual(["TWO", "three", "one"]);
  expect(root.querySelector("article em")!.textContent).toBe("body");
});

test("compiled t-out renders its body for undefined, text for strings and HTML only for Markup", async () => {
  const { root, instance } = await mountApp(RootC, templates);
  const raw = root.querySelector(".raw")!;
  expect(raw.textContent).toBe("empty");
  instance.html.set("<b>not bold</b>");
  flushSync();
  expect(raw.querySelector("b")).toBeNull();
  expect(raw.textContent).toBe("<b>not bold</b>");
  instance.html.set(markup("<b>bold</b>"));
  flushSync();
  expect(raw.querySelector("b")!.textContent).toBe("bold");
});

class MultiC extends Component {
  static override template = "contract.Multi";
  a = signal<unknown>("A");
  b = signal<unknown>("B");
}

test("a compiled multi-root template keeps its root positions working after insertion", async () => {
  const { root, instance } = await mountApp(MultiC, templates);
  expect(root.textContent).toBe("AmidB");
  instance.a.set("a2");
  instance.b.set(null);
  flushSync();
  expect(root.textContent).toBe("a2mid");
  instance.b.set("b3");
  flushSync();
  expect(root.textContent).toBe("a2midb3");
});
