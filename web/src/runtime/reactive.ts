/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Signals and the effect scheduler (§5) over the vendored
// alien-signals core. The core keeps the dependency graph; with patch 0002's hook installed, a notified
// effect comes here instead of the core's synchronous queue. We keep it in a binary heap ordered by
// (owner depth, creation sequence) and run the heap in one microtask, so a parent effect always runs
// before the queued effects of its subtree, and an effect whose owner an earlier effect disposed is a
// no-op (disposal cleared its flags).

import {
  computed as coreComputed,
  effect as coreEffect,
  endBatch,
  getActiveSub,
  runEffect,
  setActiveSub,
  setEffectScheduler,
  signal as coreSignal,
  startBatch,
} from "../../vendor/alien-signals/src/index.js";
import { ReactiveFlags, type ReactiveNode } from "../../vendor/alien-signals/src/system.js";
import { ReactiveLoopError, UIError } from "./errors.js";
import { type AppContext, MountScope, Owner, getOwner, handleError, inCallback, routeResult, setOwner } from "./owner.js";

/**
 * Cost counters (tests/costs.test.ts pins them, §9): effects created, effects kept after their first run,
 * effect runs, and signals the runtime creates for list rows and props. Plain increments.
 */
export const counters = { effectsCreated: 0, effectsKept: 0, effectRuns: 0, signals: 0 };

/**
 * Loop limits of one flush (§4). `runsPerNode`: how often one effect may run in one flush before it is
 * taken for a runaway loop (an effect that keeps writing a signal it reads, directly or through other
 * effects); a legitimate update runs each effect once, however many effects it reaches. `runsPerFlush`: a
 * backstop for a loop that never repeats an effect (each run creating and waking a new one). Internal:
 * not exported from the package; tests lower them to reach the paths quickly.
 */
export const limits = { runsPerNode: 100, runsPerFlush: 10_000_000 };

/** A core effect node with the fields the scheduler orders and counts it by. */
interface ScheduledNode extends ReactiveNode {
  erpOwner: Owner;
  erpSeq: number;
  /** The flush `erpRuns` counts in. */
  erpFlush: number;
  /** How often the node was popped in flush `erpFlush`. */
  erpRuns: number;
}

/** The core's signal: `s()` reads (and tracks), `s(v)` writes. The runtime's internal form. */
export type CoreSignal<T> = { (): T; (value: T): void };

const heap: ScheduledNode[] = [];
let sequence = 0;
let flushId = 0;
let queued = false;
let flushing = false;

function before(a: ScheduledNode, b: ScheduledNode): boolean {
  const da = a.erpOwner.depth;
  const db = b.erpOwner.depth;
  return da < db || (da === db && a.erpSeq < b.erpSeq);
}

function push(node: ScheduledNode): void {
  let i = heap.length;
  heap.push(node);
  while (i > 0) {
    const p = (i - 1) >> 1;
    const parent = heap[p]!;
    if (!before(node, parent)) break;
    heap[i] = parent;
    i = p;
  }
  heap[i] = node;
}

function pop(): ScheduledNode {
  const top = heap[0]!;
  const last = heap.pop()!;
  const n = heap.length;
  if (n > 0) {
    let i = 0;
    for (;;) {
      const l = 2 * i + 1;
      if (l >= n) break;
      const r = l + 1;
      const c = r < n && before(heap[r]!, heap[l]!) ? r : l;
      if (!before(heap[c]!, last)) break;
      heap[i] = heap[c]!;
      i = c;
    }
    heap[i] = last;
  }
  return top;
}

setEffectScheduler((node) => {
  push(node as ScheduledNode);
  if (!queued && !flushing) {
    queued = true;
    queueMicrotask(flush);
  }
});

const STALE = ReactiveFlags.Dirty | ReactiveFlags.Pending | ReactiveFlags.Recursed | ReactiveFlags.RecursedCheck;

/**
 * The owners whose effects ran in the current round of the flush; filled only while a flush runs, so a
 * mount (which runs outside any flush) never leaks into a later, unrelated flush (review round 3, R8).
 */
const dirtiedOwners = new Set<Owner>();

/** Called after each round of a flush with the owners whose effects ran in it (onFlushed). */
export type FlushCallback = (dirtiedOwners: ReadonlySet<Owner>) => void;
const flushListeners = new Set<FlushCallback>();

/** Registers a listener called after each round of a flush; returns its unregister function. */
export function onFlushComplete(cb: FlushCallback): () => void {
  flushListeners.add(cb);
  return () => {
    flushListeners.delete(cb);
  };
}

function notifyFlushListeners(): void {
  if (dirtiedOwners.size === 0) return;
  const dirty = new Set(dirtiedOwners);
  dirtiedOwners.clear();
  for (const listener of [...flushListeners]) {
    try {
      listener(dirty);
    } catch (e) {
      reportUnhandled(e);
    }
  }
}

/**
 * Makes a node that will not run in this flush notifiable again by its next dependency change: watching,
 * neither dirty nor pending (the core skips a node that is already dirty or pending). A disposed node
 * (flags 0) stays disposed.
 */
function revive(node: ScheduledNode): void {
  if (node.flags !== ReactiveFlags.None) node.flags = (node.flags & ~STALE) | ReactiveFlags.Watching;
}

function flush(): void {
  queued = false;
  if (flushing) return;
  flushing = true;
  const id = ++flushId;
  let total = 0;
  try {
    // A flush runs rounds until nothing is queued: the effects, then the flush listeners (onFlushed),
    // whose signal writes queue effects that run in the next round of the same flush, never stranded
    // (review round 3, R2). The rounds share the flush's loop limits, so an onFlushed that keeps waking
    // its own bindings stops at `runsPerNode` like any other loop.
    do {
      while (heap.length > 0) {
        const node = pop();
        if (node.flags === ReactiveFlags.None) continue; // disposed after it was queued
        if (node.erpFlush !== id) {
          node.erpFlush = id;
          node.erpRuns = 0;
        }
        if (++node.erpRuns > limits.runsPerNode) {
          revive(node);
          handleError(
            node.erpOwner,
            new ReactiveLoopError(
              `an effect ran more than ${limits.runsPerNode} times in one flush: it keeps writing a signal it reads; it was stopped for this flush and runs again at the next change of what it reads`,
            ),
          );
          continue;
        }
        if (++total > limits.runsPerFlush) {
          // The queued effects of every app are dropped, not only those of this node's subtree, so no
          // boundary may take the error: each app that lost effects crashes (and reports it), so none
          // stays on screen with stale bindings (review round 2, silent-failure 9).
          const apps = new Set<AppContext>([node.erpOwner.app]);
          revive(node);
          while (heap.length > 0) {
            const dropped = heap.pop()!;
            revive(dropped);
            apps.add(dropped.erpOwner.app);
          }
          const error = new ReactiveLoopError(
            `a flush ran more than ${limits.runsPerFlush} effects without repeating one: effects keep creating and waking new effects; the queued effects were dropped and the app stopped`,
          );
          for (const app of apps) {
            try {
              app.crash(error);
            } catch (e) {
              reportUnhandled(e);
            }
          }
          break;
        }
        try {
          dirtiedOwners.add(node.erpOwner);
          runEffect(node);
        } catch (e) {
          // The effect function itself never throws (createEffect wraps it), but the core's dirty check
          // evaluates computeds before it runs: a getter the runtime did not wrap (computed() boxes its
          // own errors) lands here. The node lost its watching flag when it was queued; revive it so the
          // next change runs it again, and route the error as the effect's own.
          revive(node);
          handleError(node.erpOwner, e);
        }
      }
      notifyFlushListeners();
    } while (heap.length > 0);
  } finally {
    dirtiedOwners.clear();
    flushing = false;
  }
}

/** Runs every queued effect now (tests, the benchmark harness, the root mount). */
export function flushSync(): void {
  flush();
}

/**
 * A readable, writable signal: `s()` reads (and tracks), `s.set(v)` writes. `set` is a read-only
 * function-typed property, not a method, so `Signal<T>` is invariant in `T` (a `Signal<string>` is not a
 * `Signal<string | number>`) and the setter cannot be replaced (type review M1). A `T` that includes
 * `void` lets `s.set()` type-check, and the core reads instead of writing when called with no argument:
 * write `s.set(undefined)` (type review L4, pinned in tests/types.test.ts).
 */
export interface Signal<T> {
  (): T;
  readonly set: (value: T) => void;
}

/** A signal holding `value`; a write notifies only when the new value is not `===` the old one. */
export function signal<T>(value: T): Signal<T> {
  const s = coreSignal<T>(value);
  // The core's bound accessor writes when called with an argument, so it is its own setter: no closure
  // per signal.
  (s as unknown as { set: (value: T) => void }).set = s;
  return s as unknown as Signal<T>;
}

/** Carries an error a computed getter threw past the core, which must never see an exception (§5). */
class ComputedFailure {
  constructor(readonly error: unknown) {}
}

/**
 * A lazily evaluated, cached derivation; it notifies its readers only when its value changes. A getter
 * that throws makes every read throw that error (the reader's error chain gets it); the core itself
 * never sees the exception, so its dirty check stays sound (a failure is always a new value, so the
 * readers re-run and see it).
 */
export function computed<T>(fn: () => T): () => T {
  const c = coreComputed<T | ComputedFailure>(() => {
    try {
      return fn();
    } catch (e) {
      return new ComputedFailure(e);
    }
  });
  return () => {
    const v = c();
    if (v instanceof ComputedFailure) throw v.error;
    return v;
  };
}

/** Runs `fn` with every signal write inside grouped into one propagation (effects still run in the microtask). */
export function batch<T>(fn: () => T): T {
  startBatch();
  try {
    return fn();
  } finally {
    endBatch();
  }
}

/** Runs `fn` without recording what it reads as a dependency of the running computation. */
export function untrack<T>(fn: () => T): T {
  const prev = setActiveSub(undefined);
  try {
    return fn();
  } finally {
    setActiveSub(prev);
  }
}

/** Options of `createEffect`. */
export interface EffectOptions {
  /**
   * Keep the effect even when its first run tracked nothing, so a cleanup it returned runs at disposal
   * and not at once. The public `effect()` and `useEffect()` keep; compiled bindings and the runtime's
   * structure effects do not (a constant binding costs one run and no memory).
   */
  readonly keepAlive: boolean;
}

const BINDING: EffectOptions = { keepAlive: false };

/** `createEffect` options of the public effects (`effect`, `useEffect`). */
export const KEEP: EffectOptions = { keepAlive: true };

/**
 * Creates an effect owned by `owner` and runs it once now. The function runs with `owner` current;
 * anything it throws goes to `handleError(owner, …)`, so the core never sees an exception. A function
 * returned by `fn` is a cleanup, run before the next run and at disposal; a returned promise's rejection
 * goes to the owner (or the app's report channel once it is disposed). Unless `options.keepAlive`, an
 * effect that tracked nothing in its first run disposes itself and is not stored. Returns
 * the effect's stop function, or null when it was not kept.
 */
export function createEffect(fn: () => unknown, owner: Owner, options: EffectOptions = BINDING): (() => void) | null {
  if (owner.disposed) return null;
  counters.effectsCreated++;
  let node: ScheduledNode | undefined;
  const run = (): (() => void) | undefined => {
    counters.effectRuns++;
    if (node === undefined) {
      node = getActiveSub() as ScheduledNode;
      node.erpOwner = owner;
      node.erpSeq = sequence++;
      node.erpFlush = 0;
      node.erpRuns = 0;
    }
    const prevOwner = setOwner(owner);
    try {
      const result = fn();
      // Compiled bindings return undefined: the hot path pays one comparison.
      if (result === undefined) return undefined;
      if (typeof result === "function") return wrapCleanup(result as () => unknown, owner);
      // An async effect: its rejection reaches the owner (review round 2, silent-failure 3).
      routeResult(result, owner);
      return undefined;
    } catch (e) {
      handleError(owner, e);
      return undefined;
    } finally {
      setOwner(prevOwner);
    }
  };
  const prevSub = setActiveSub(undefined);
  let stop: () => void;
  try {
    stop = coreEffect(run);
  } finally {
    setActiveSub(prevSub);
  }
  if (node!.deps === undefined && !options.keepAlive) {
    stop();
    return null;
  }
  if (owner.disposed) {
    // The first run disposed its own owner (an error boundary took over): never keep it.
    stop();
    return null;
  }
  (owner.effects ??= []).push(stop);
  counters.effectsKept++;
  return stop;
}

function wrapCleanup(cleanup: () => unknown, owner: Owner): () => void {
  return () => {
    try {
      cleanup();
    } catch (e) {
      handleError(owner, e);
    }
  };
}

/** The app of effects created with no component current: errors are reported, never dropped. */
const detachedApp: AppContext = {
  get root(): Element {
    throw new UIError("an effect created outside a component has no render root", "usage");
  },
  delegated: new Set(),
  crash(error: unknown): void {
    reportUnhandled(error);
  },
  report(error: unknown): void {
    reportUnhandled(error);
  },
  template(): undefined {
    return undefined;
  },
};

/** Reports an error nobody handled: `globalThis.reportError` where it exists, else an async rethrow. */
export function reportUnhandled(error: unknown): void {
  const report = (globalThis as { reportError?: (e: unknown) => void }).reportError;
  if (typeof report === "function") {
    report(error);
  } else {
    queueMicrotask(() => {
      throw error;
    });
  }
}

/** What a public effect may return: nothing, or a cleanup. */
export type EffectResult = void | (() => void);

/**
 * The public `effect(fn)`: owned by the current owner (disposed with its component) or, outside every
 * component, by a detached owner the returned function disposes. `fn` may return a cleanup; the effect is
 * kept even when it reads no signal, so the cleanup runs when the effect is stopped, never at once.
 * Refused (review round 2, silent-failure 10): under an owner already disposed (it would never run), and
 * inside a component callback with no current owner (an event handler, a lifecycle hook, an `onError`
 * handler), where it would outlive the component and report its errors outside its chain: create it in
 * `setup()` with `useEffect`, or while rendering.
 */
export function effect(fn: () => EffectResult): () => void {
  if (typeof fn !== "function") throw new UIError(`effect takes a function, got ${typeof fn}`, "handler");
  const parent = getOwner();
  if (parent !== null) {
    if (parent.disposed) {
      throw new UIError("effect() was called under a component that is already destroyed: the effect would never run", "usage");
    }
    // Null only when the first run disposed its own owner (its error went to a boundary): nothing is
    // left to stop.
    return createEffect(fn, parent, KEEP) ?? alreadyStopped;
  }
  if (inCallback()) {
    throw new UIError(
      "effect() was called from an event handler or a lifecycle hook, where no component owns it: create it in setup() with useEffect, or while rendering",
      "usage",
    );
  }
  const owner = new Owner(null, detachedApp, null, new MountScope());
  createEffect(fn, owner, KEEP);
  return () => {
    const effects = owner.effects;
    owner.effects = null;
    if (effects) for (const stop of effects) stop();
  };
}

/** The stop function of an effect its own first run already disposed (through its owner). */
function alreadyStopped(): void {}
