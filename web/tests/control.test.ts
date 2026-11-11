/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Structural helpers (§7.3): cond, keyed, list.

import { expect, test } from "vitest";
import * as $ from "@erpforall/elang/abi";
import { ErrorBoundary, type Row, UIError, flushSync, onError, onWillDestroy, signal } from "../src/index.js";
import { type Self, comp, mountApp } from "./harness.js";

async function view(render: (self: Self) => Node | null, errors?: unknown[]) {
  const { C, t } = comp("T", render, errors ? { setup: () => onError((e) => void errors.push(e)) } : {});
  return (await mountApp(C, [t])).root;
}

test("cond builds only the selected branch and rebuilds only when the index changes", async () => {
  const which = signal(0);
  const label = signal("a");
  let builds = 0;
  const root = await view(() => {
    const n = ($.template("<div><!>|</div>", 0)() as Element);
    $.cond(n, n.firstChild, () => (which() === 0 ? 0 : which() === 1 ? 1 : -1), [
      () => {
        builds++;
        const p = ($.template("<p></p>", 0)() as Element);
        $.insert(p, () => label(), null);
        return p;
      },
      () => $.template("<i>else</i><b>2</b>", 1)(),
    ]);
    return n;
  });
  const div = root.firstElementChild!;
  expect(div.innerHTML).toBe("<p>a</p><!---->|");
  label.set("b"); // inside the branch: no rebuild
  flushSync();
  expect(builds).toBe(1);
  expect(div.innerHTML).toBe("<p>b</p><!---->|");
  which.set(1);
  flushSync();
  expect(div.textContent).toBe("else2|");
  which.set(2);
  flushSync();
  expect(div.textContent).toBe("|");
  which.set(0);
  flushSync();
  expect(builds).toBe(2);
  expect(div.textContent).toBe("b|");
});

test("cond: a branch's data stays text (security §8)", async () => {
  const root = await view(() => {
    const n = ($.template("<div><!></div>", 0)() as Element);
    $.cond(n, n.firstChild, () => 0, [
      () => {
        const p = ($.template("<p></p>", 0)() as Element);
        $.insert(p, () => "<img src=x onerror=alert(1)>", null);
        return p;
      },
    ]);
    return n;
  });
  expect(root.querySelector("img")).toBeNull();
  expect(root.textContent).toBe("<img src=x onerror=alert(1)>");
});

test("keyed rebuilds its block only when the key changes", async () => {
  const key = signal<unknown>(1);
  const other = signal(0);
  let builds = 0;
  const root = await view(() => {
    const n = ($.template("<div></div>", 0)() as Element);
    $.keyed(n, null, () => key(), () => {
      builds++;
      const p = ($.template("<p></p>", 0)() as Element);
      $.insert(p, () => `${String(key())}:${other()}`, null);
      return p;
    });
    return n;
  });
  const first = root.querySelector("p");
  other.set(1);
  flushSync();
  expect(builds).toBe(1);
  expect(root.querySelector("p")).toBe(first);
  key.set(2);
  flushSync();
  expect(builds).toBe(2);
  expect(root.querySelector("p")).not.toBe(first);
  expect(root.textContent).toBe("2:1");
  key.set(NaN);
  flushSync();
  key.set(NaN); // Object.is: same key
  flushSync();
  expect(builds).toBe(3);
});

test("keyed: the block's data stays text (security §8)", async () => {
  const root = await view(() => {
    const n = ($.template("<div></div>", 0)() as Element);
    $.keyed(n, null, () => "</div><script>alert(1)</script>", () => {
      const p = ($.template("<p></p>", 0)() as Element);
      $.insert(p, () => "</div><script>alert(1)</script>", null);
      return p;
    });
    return n;
  });
  expect(root.querySelector("script")).toBeNull();
  expect(root.textContent).toBe("</div><script>alert(1)</script>");
});

function rowView(items: () => unknown, onRow?: (r: Row) => void) {
  return () => {
    const n = ($.template("<ul></ul>", 0)() as Element);
    $.list(
      n,
      null,
      items,
      (item) => (typeof item === "object" && item !== null ? (item as { id: unknown }).id : item),
      (r) => {
        onRow?.(r);
        const li = ($.template("<li></li>", 0)() as Element);
        $.insert(li, () => {
          const it = r.item();
          const v = typeof it === "object" && it !== null ? (it as { label: string }).label : String(it);
          return `${r.index()}/${r.count()}:${v}`;
        }, null);
        return li;
      },
    );
    return n;
  };
}

/// (partial: runtime half, every collection kind t-foreach iterates)
test("list iterates arrays, Maps, Sets, strings and plain objects as OWL's prepareList does", async () => {
  const coll = signal<unknown>(["a", "b"]);
  const values: unknown[] = [];
  const root = await (async () => {
    const { C, t } = comp("T", () => {
      const n = ($.template("<p></p>", 0)() as Element);
      $.list(n, null, () => coll(), (item) => item, (r) => {
        const s = ($.template("<s></s>", 0)() as Element);
        $.insert(s, () => `${String(r.item())}=${String(r.value())}`, null);
        values.push(r.value());
        return s;
      });
      return n;
    });
    return (await mountApp(C, [t])).root;
  })();
  const text = () => [...root.querySelectorAll("s")].map((s) => s.textContent);
  expect(text()).toEqual(["a=a", "b=b"]);
  coll.set(new Map([["k1", 1], ["k2", 2]]));
  flushSync();
  expect(text()).toEqual(["k1=1", "k2=2"]);
  coll.set(new Set(["x", "y"]));
  flushSync();
  expect(text()).toEqual(["x=x", "y=y"]);
  coll.set("hi");
  flushSync();
  expect(text()).toEqual(["h=h", "i=i"]);
  coll.set({ one: 1, two: 2 });
  flushSync();
  expect(text()).toEqual(["one=1", "two=2"]);
});

/// (partial: runtime half, the row's index and count metadata)
test("list rows expose index and count, updated in place on reorder and resize", async () => {
  const items = signal<unknown>([1, 2, 3]);
  const root = await view(rowView(() => items()));
  const text = () => [...root.querySelectorAll("li")].map((li) => li.textContent);
  expect(text()).toEqual(["0/3:1", "1/3:2", "2/3:3"]);
  const li3 = root.querySelectorAll("li")[2];
  items.set([3, 1, 2, 4]);
  flushSync();
  expect(text()).toEqual(["0/4:3", "1/4:1", "2/4:2", "3/4:4"]);
  expect(root.querySelectorAll("li")[0]).toBe(li3); // kept by key, moved
});

/// (partial: runtime half, duplicate keys fail loudly and leave the rows as they were)
test("a duplicate key is an error in every mode and changes nothing", async () => {
  const errors: unknown[] = [];
  const items = signal<unknown>([1, 2]);
  const root = await view(rowView(() => items()), errors);
  items.set([1, 2, 1]);
  flushSync();
  expect((errors[0] as Error).message).toBe("Got duplicate key in t-foreach: 1");
  expect([...root.querySelectorAll("li")].map((li) => li.textContent)).toEqual(["0/2:1", "1/2:2"]);
});

/// (partial: runtime half, a non-iterable collection is OWL's error)
test("a collection that is not iterable is OWL's error, also for null, undefined and numbers", async () => {
  for (const bad of [null, undefined, 42, true]) {
    const errors: unknown[] = [];
    await view(rowView(() => bad), errors);
    expect((errors[0] as Error).message).toBe(`Invalid loop expression: "${String(bad)}" is not iterable`);
  }
});

test("list keeps rows by key: kept rows keep their nodes and bindings, removed rows are disposed", async () => {
  const items = signal<unknown>([
    { id: 1, label: "one" },
    { id: 2, label: "two" },
    { id: 3, label: "three" },
  ]);
  let built = 0;
  const root = await view(rowView(() => items(), () => built++));
  const lis = () => [...root.querySelectorAll("li")];
  const [one, , three] = lis();
  items.set([
    { id: 3, label: "THREE" },
    { id: 1, label: "one" },
  ]);
  flushSync();
  expect(built).toBe(3);
  expect(lis()).toEqual([three, one]);
  expect(lis().map((li) => li.textContent)).toEqual(["0/2:THREE", "1/2:one"]);
  items.set([]);
  flushSync();
  expect(root.querySelector("ul")!.childNodes).toHaveLength(0);
  items.set([{ id: 9, label: "nine" }]);
  flushSync();
  expect(built).toBe(4);
  expect(lis().map((li) => li.textContent)).toEqual(["0/1:nine"]);
});

test("list with a marker and multi-node rows reconciles inside its own range", async () => {
  const items = signal<unknown>(["a", "b", "c"]);
  const root = await view(() => {
    const n = ($.template("<div>[<!>]</div>", 0)() as Element);
    const m = n.firstChild!.nextSibling!;
    $.list(n, m, () => items(), (item) => item, (r) => {
      const f = $.template("<i></i><b>,</b>", 1)();
      $.insert(f.firstChild!, () => r.item(), null);
      return f;
    });
    return n;
  });
  const div = root.firstElementChild!;
  expect(div.textContent).toBe("[a,b,c,]");
  items.set(["c", "a", "d"]);
  flushSync();
  expect(div.textContent).toBe("[c,a,d,]");
  items.set([]);
  flushSync();
  expect(div.textContent).toBe("[]");
  items.set(["z"]);
  flushSync();
  expect(div.textContent).toBe("[z,]");
});

test("list: items holding markup render as text (security §8)", async () => {
  const root = await view(rowView(() => ["<img src=x onerror=alert(1)>", "<script>alert(1)</script>"]));
  expect(root.querySelector("img, script")).toBeNull();
  expect(root.querySelectorAll("li")).toHaveLength(2);
  expect(root.querySelector("li")!.textContent).toBe("0/2:<img src=x onerror=alert(1)>");
});

test("cond: a branch that throws goes to the error chain; an index out of range or NaN is a value error", async () => {
  const errors: unknown[] = [];
  const which = signal<number>(-1);
  const root = await view(() => {
    const n = ($.template("<div><!></div>", 0)() as Element);
    $.cond(n, n.firstChild, () => which(), [
      () => {
        throw new Error("branch failed");
      },
      () => ($.template("<b>ok</b>", 0)() as Element),
    ]);
    return n;
  }, errors);
  which.set(0);
  flushSync();
  expect(errors.map((e) => (e as Error).message)).toEqual(["branch failed"]);
  which.set(1);
  flushSync();
  expect(root.querySelector("b")!.textContent).toBe("ok");
  for (const bad of [2, -2, Number.NaN, 0.5]) {
    which.set(bad);
    flushSync();
    expect(errors.at(-1)).toEqual(new UIError(`cond: branch index ${String(bad)} is out of range (0..1 or -1)`, "value"));
    expect(root.querySelector("b")!.textContent).toBe("ok"); // the shown branch stays
  }
});

test("keyed compares keys with Object.is: NaN equals NaN, 0 and -0 differ, objects by identity", async () => {
  // The key sits in a fresh object per write, so every write notifies (a signal itself treats 0 and -0
  // as equal) and only keyed's own comparison decides.
  const key = signal<{ k: unknown }>({ k: Number.NaN });
  let builds = 0;
  await view(() => {
    const n = ($.template("<div><!></div>", 0)() as Element);
    $.keyed(n, n.firstChild, () => key().k, () => {
      builds++;
      return ($.template("<b></b>", 0)() as Element);
    });
    return n;
  });
  const steps: Array<[unknown, number]> = [];
  const obj = { id: 1 };
  for (const k of [Number.NaN, 0, -0, -0, obj, obj, { id: 1 }]) {
    key.set({ k });
    flushSync();
    steps.push([k, builds]);
  }
  expect(steps.map(([, b]) => b)).toEqual([1, 2, 3, 3, 4, 4, 5]);
});

test("list keys may be objects or NaN; a duplicate object key is refused like any other", async () => {
  const errors: unknown[] = [];
  const a = { id: "a" };
  const b = { id: "b" };
  const items = signal<unknown[]>([a, b, Number.NaN]);
  const root = await view(() => {
    const n = ($.template("<ul></ul>", 0)() as Element);
    $.list(n, null, () => items(), (item) => item, (r) => {
      const li = ($.template("<li></li>", 0)() as Element);
      $.insert(li, () => String((r.item() as { id?: string })?.id ?? r.item()), null);
      return li;
    });
    return n;
  }, errors);
  const lis = () => [...root.querySelectorAll("li")];
  const [la, lb, ln] = lis();
  items.set([Number.NaN, b, a]);
  flushSync();
  expect(lis()).toEqual([ln, lb, la]); // every row kept by key, NaN included
  items.set([a, a]);
  flushSync();
  expect(errors).toEqual([new UIError("Got duplicate key in t-foreach: [object Object]", "loop")]);
  expect(lis()).toEqual([ln, lb, la]);
});

test("rows a list built in an update that an error boundary cut short are disposed (review M3)", async () => {
  const log: string[] = [];
  const tick = signal(0);
  const items = signal<number[]>([1]);
  const row = comp("Row", ($self) => {
    const li = ($.template("<li></li>", 0)() as Element);
    if ($self.props.id === 3) throw new Error("row 3 failed");
    $.insert(li, () => `${String($self.props.id)}:${tick()}`, null);
    return li;
  }, { setup: (self) => onWillDestroy(() => void log.push(`destroy ${String(self.props.id)}`)) });
  const page = comp("Page", ($self) => {
    const n = ($.template("<main></main>", 0)() as Element);
    $.component(n, null, $self, "ErrorBoundary", {}, null, {
      default: {
        render: () => {
          const ul = ($.template("<ul></ul>", 0)() as Element);
          $.list(ul, null, () => items(), (item) => item, (r) => {
            const f = $.template("<!>", 1)();
            $.component(f, f.firstChild, $self, "Row", { id: () => r.item() }, null, null);
            return f;
          });
          return ul;
        },
      },
      fallback: { render: () => ($.template("<em>failed</em>", 0)() as Element) },
    });
    return n;
  }, { components: { ErrorBoundary, Row: row.C } });
  const { root } = await mountApp(page.C, [page.t, row.t]);
  expect(root.querySelector("ul")!.textContent).toBe("1:0");
  items.set([1, 2, 3]);
  flushSync();
  expect(root.querySelector("em")!.textContent).toBe("failed");
  expect(log.sort()).toEqual(["destroy 1", "destroy 2", "destroy 3"]);
});
