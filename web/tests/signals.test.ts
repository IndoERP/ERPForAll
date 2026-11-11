/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Signals and the scheduler.

import { expect, test } from "vitest";
import { computed as coreComputed } from "../vendor/alien-signals/src/index.js";
import { ReactiveLoopError, batch, computed, effect, flushSync, onError, signal, untrack } from "../src/index.js";
import { createEffect, limits } from "../src/runtime/reactive.js";
import { type Owner, dispose, getOwner } from "../src/runtime/owner.js";
import { comp, mountApp } from "./harness.js";

/**
 * The owner of a mounted component whose `onError` collects `errors` (the real owner tree: an app, a
 * component, its boundary and mount scope). Child owners come from `owner.child()`, as structures make them.
 */
async function componentOwner(errors: unknown[] = []): Promise<Owner> {
  let owner: Owner | null = null;
  const { C, t } = comp("O", () => null, {
    setup: () => {
      owner = getOwner();
      onError((e) => void errors.push(e));
    },
  });
  await mountApp(C, [t]);
  return owner!;
}

/** Runs `fn` with `limits` changed, restoring them afterwards. */
function withLimits(change: Partial<typeof limits>, fn: () => void): void {
  const saved = { ...limits };
  Object.assign(limits, change);
  try {
    fn();
  } finally {
    Object.assign(limits, saved);
  }
}

test("a signal read in an effect subscribes it, and a write notifies it once per changed value", async () => {
  const s = signal(1);
  const seen: number[] = [];
  const stop = effect(() => {
    seen.push(s());
  });
  expect(seen).toEqual([1]);
  s.set(2);
  expect(seen).toEqual([1]); // effects run in the microtask
  await Promise.resolve();
  expect(seen).toEqual([1, 2]);
  s.set(2); // same value: no notification
  s.set(3);
  flushSync();
  expect(seen).toEqual([1, 2, 3]);
  expect(s()).toBe(3);
  stop();
  s.set(4);
  flushSync();
  expect(seen).toEqual([1, 2, 3]);
});

test("a computed evaluates lazily, caches until a dependency changes and notifies only on a new value", () => {
  const a = signal(2);
  let runs = 0;
  const parity = computed(() => {
    runs++;
    return a() % 2;
  });
  expect(runs).toBe(0); // lazy
  expect(parity()).toBe(0);
  expect(parity()).toBe(0);
  expect(runs).toBe(1); // cached
  const seen: number[] = [];
  const stop = effect(() => {
    seen.push(parity());
  });
  a.set(4); // parity stays 0: the computed re-runs, the effect does not
  flushSync();
  expect(runs).toBe(2);
  expect(seen).toEqual([0]);
  a.set(5);
  flushSync();
  expect(seen).toEqual([0, 1]);
  stop();
});

test("an effect re-subscribes on each run, runs its cleanup before re-running and on disposal", () => {
  const useA = signal(true);
  const a = signal("a1");
  const b = signal("b1");
  const log: string[] = [];
  const stop = effect(() => {
    const v = useA() ? a() : b();
    log.push(`run ${v}`);
    return () => log.push(`cleanup ${v}`);
  });
  useA.set(false);
  flushSync();
  expect(log).toEqual(["run a1", "cleanup a1", "run b1"]);
  a.set("a2"); // no longer a dependency
  flushSync();
  expect(log).toEqual(["run a1", "cleanup a1", "run b1"]);
  b.set("b2");
  flushSync();
  expect(log.slice(3)).toEqual(["cleanup b1", "run b2"]);
  stop();
  expect(log.slice(5)).toEqual(["cleanup b2"]);
  b.set("b3");
  flushSync();
  expect(log).toHaveLength(6);
});

test("untrack reads without subscribing the running effect and returns the value", () => {
  const tracked = signal(1);
  const hidden = signal(10);
  const seen: number[] = [];
  const stop = effect(() => {
    seen.push(tracked() + untrack(() => hidden()));
  });
  hidden.set(20);
  flushSync();
  expect(seen).toEqual([11]);
  tracked.set(2);
  flushSync();
  expect(seen).toEqual([11, 22]);
  stop();
});

test("batch groups writes: the effect sees only the final values, once", async () => {
  const a = signal(1);
  const b = signal(1);
  const seen: string[] = [];
  const stop = effect(() => {
    seen.push(`${a()}-${b()}`);
  });
  const r = batch(() => {
    a.set(2);
    b.set(3);
    return "done";
  });
  expect(r).toBe("done");
  await Promise.resolve();
  expect(seen).toEqual(["1-1", "2-3"]);
  stop();
});

test("set is the signal's own accessor: no closure per signal, and it writes", () => {
  const s = signal(1);
  expect(s.set).toBe(s);
  s.set(5);
  expect(s()).toBe(5);
});

test("a compiled binding that tracked nothing disposes itself after its first run", async () => {
  const owner = await componentOwner();
  let runs = 0;
  const kept = createEffect(() => {
    runs++;
  }, owner);
  expect(runs).toBe(1);
  expect(kept).toBeNull();
  expect(owner.effects).toBeNull();
  const s = signal(0);
  expect(createEffect(() => s(), owner)).not.toBeNull();
  expect(owner.effects).toHaveLength(1);
});

test("a public effect that reads no signal keeps its cleanup until it is stopped (review H2)", () => {
  const log: string[] = [];
  const stop = effect(() => {
    log.push("added listener");
    return () => log.push("removed listener");
  });
  expect(log).toEqual(["added listener"]); // not undone at creation
  stop();
  expect(log).toEqual(["added listener", "removed listener"]);
});

test("queued effects run parents before children, then in creation order", async () => {
  const root = await componentOwner();
  const child = root.child();
  const grandchild = child.child();
  const s = signal(0);
  const order: string[] = [];
  createEffect(() => {
    s();
    order.push("grandchild");
  }, grandchild);
  createEffect(() => {
    s();
    order.push("child-1");
  }, child);
  createEffect(() => {
    s();
    order.push("root");
  }, root);
  createEffect(() => {
    s();
    order.push("child-2");
  }, child);
  order.length = 0;
  s.set(1);
  flushSync();
  expect(order).toEqual(["root", "child-1", "child-2", "grandchild"]);
});

test("a parent effect that disposes a subtree cancels the subtree's queued effects", async () => {
  const root = await componentOwner();
  const child = root.child();
  root.onCleanup(() => dispose(child));
  const s = signal(0);
  const order: string[] = [];
  createEffect(() => {
    s();
    order.push("child");
  }, child);
  createEffect(() => {
    if (s() === 1) dispose(child);
    order.push("parent");
  }, root);
  order.length = 0;
  s.set(1);
  flushSync();
  expect(order).toEqual(["parent"]);
});

test("an error in an effect goes to its owner's error chain and the flush goes on", async () => {
  const errors: unknown[] = [];
  const owner = await componentOwner(errors);
  const s = signal(0);
  const seen: number[] = [];
  createEffect(() => {
    if (s() === 1) throw new Error("boom");
  }, owner);
  createEffect(() => {
    seen.push(s());
  }, owner);
  s.set(1);
  flushSync();
  expect(errors).toHaveLength(1);
  expect((errors[0] as Error).message).toBe("boom");
  expect(seen).toEqual([0, 1]);
  s.set(2); // the failing effect is still live
  flushSync();
  expect(seen).toEqual([0, 1, 2]);
});

test("a throwing computed reaches the reading effect's error chain, and the effect runs again later (review H3)", async () => {
  const errors: unknown[] = [];
  const owner = await componentOwner(errors);
  const s = signal(0);
  const c = computed(() => {
    if (s() === 1) throw new Error("getter failed");
    return s() * 10;
  });
  const seen: number[] = [];
  createEffect(() => {
    seen.push(c());
  }, owner);
  s.set(1);
  flushSync();
  expect(errors).toHaveLength(1);
  expect((errors[0] as Error).message).toBe("getter failed");
  expect(() => c()).toThrow("getter failed"); // every read throws while the getter fails
  s.set(2);
  flushSync();
  expect(seen).toEqual([0, 20]);
});

test("an error thrown in the core's dirty check goes to the effect's owner, and the effect is revived (review H3)", async () => {
  const errors: unknown[] = [];
  const owner = await componentOwner(errors);
  const s = signal(0);
  // A core computed the runtime did not wrap: its getter throws inside the dirty check, before the
  // effect function runs.
  const raw = coreComputed(() => {
    if (s() === 1) throw new Error("raw getter failed");
    return s();
  });
  const later = signal(0);
  const seen: number[] = [];
  createEffect(() => {
    seen.push(raw());
  }, owner);
  createEffect(() => {
    later();
  }, owner);
  s.set(1);
  later.set(1);
  expect(() => flushSync()).not.toThrow();
  expect(errors).toHaveLength(1);
  expect((errors[0] as Error).message).toBe("raw getter failed");
  s.set(2);
  flushSync();
  expect(seen).toEqual([0, 2]);
});

test("one write that reaches more than 100,000 bindings updates every one of them (review H4)", async () => {
  const errors: unknown[] = [];
  const owner = await componentOwner(errors);
  const s = signal(0);
  const n = 120_000;
  let sum = 0;
  for (let i = 0; i < n; i++) {
    createEffect(() => {
      sum += s();
    }, owner);
  }
  s.set(1);
  flushSync();
  expect(errors).toEqual([]);
  expect(sum).toBe(n);
  dispose(owner);
});

test("a runaway effect is stopped after the per-node limit, the flush goes on, and it runs again later (review H4)", async () => {
  const errors: unknown[] = [];
  const owner = await componentOwner(errors);
  // Two effects that write what the other reads (an effect writing what it reads itself is a recursion
  // the core settles without re-queuing it).
  const x = signal(0);
  const y = signal(0);
  let looping = true;
  let runs = 0;
  createEffect(() => {
    runs++;
    const v = x();
    if (looping) y.set(v + 1);
  }, owner);
  createEffect(() => {
    const v = y();
    if (looping) x.set(v + 1);
  }, owner);
  const other = signal(0);
  const seen: number[] = [];
  createEffect(() => {
    seen.push(other());
  }, owner);
  runs = 0;
  other.set(1);
  flushSync();
  expect(errors).toHaveLength(1);
  expect(errors[0]).toBeInstanceOf(ReactiveLoopError);
  expect((errors[0] as ReactiveLoopError).code).toBe("reactive-loop");
  expect((errors[0] as Error).message).toBe(
    `an effect ran more than ${limits.runsPerNode} times in one flush: it keeps writing a signal it reads; it was stopped for this flush and runs again at the next change of what it reads`,
  );
  expect(runs).toBe(limits.runsPerNode);
  expect(seen).toEqual([0, 1]); // the rest of the flush ran
  looping = false;
  runs = 0;
  x.set(5);
  flushSync();
  expect(runs).toBe(1); // revived: the next change runs it again
  expect(errors).toHaveLength(1);
});

// The flush backstop (a chain that never repeats an effect) is in tests/routing.test.ts: since review round 2
// it stops every app that lost effects instead of reporting to one owner.
