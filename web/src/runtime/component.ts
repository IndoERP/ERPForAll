/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Components, hooks, slots, mount and the built-in boundaries (§7.5,
// §7.6). OWL's component model (class, static template and components, setup, hooks, onError) rebuilt
// on owners and signals: a component renders once; later changes reach the DOM through the effects of
// its bindings.

import { trackTestWork } from "./test-mutations.js";
import { createEnv, validateEnv, validateEnvOptions, type Env } from "./env.js";
import { ExtensionError } from "./extension-error.js";
import type { ServiceClass, Services } from "./services.js";
import { signal as coreSignal } from "../../vendor/alien-signals/src/index.js";
import type { Slot } from "./abi.js";
import { Region, detached, endAnchor, endBuild, insertBefore, removeRange } from "./block.js";
import { UIError } from "./errors.js";
import { list } from "./control.js";
import {
  type AppContext,
  Boundary,
  type ComponentState,
  type Hook,
  MountScope,
  Owner,
  dispose,
  enterCallback,
  handleError,
  isThenable,
  leaveCallback,
  mountTree,
  requireOwner,
  routeError,
  routeResult,
  setOwner,
} from "./owner.js";
import { type PropsSchema, isPropsSchema, validateProp, validateProps } from "./props.js";
import {
  type CoreSignal,
  type EffectResult,
  KEEP,
  batch,
  computed,
  createEffect,
  onFlushComplete,
  reportUnhandled,
  untrack,
} from "./reactive.js";

export type { Slot };

/** A compiled template module: `export const name`, `export function render($self)`. */
export interface TemplateModule {
  readonly name: string;
  readonly render: (self: object) => Node | null;
}

/** Built-in components render through this static method instead of a template (module-private). */
const RENDER: unique symbol = Symbol("erp.render");

type BuiltinRender = (cs: ComponentScope, parent: Node, end: Node | null) => void;

/**
 * A component class. The constructor takes `never` so every subclass fits whatever its props type
 * (`Component<{a: number}>` in `static components` included); the runtime builds the props object.
 */
export interface ComponentClass {
  new (props: never): Component<object>;
  readonly name: string;
  template?: string;
  components?: Record<string, ComponentClass>;
}

/** A built-in component class (`Pending`, `ErrorBoundary`): internal, never in the public types (type review L5). */
interface BuiltinClass extends ComponentClass {
  readonly [RENDER]?: BuiltinRender;
}

/**
 * What `setup()` and the synchronous hooks return: nothing. A union, not a bare `void`, so an `async`
 * function (which returns a promise) does not type-check where only a synchronous one keeps its meaning
 * (type review H2); the runtime refuses or routes a promise there too, for plain-JS addons.
 */
export type SyncResult = void | null;

/**
 * The props type of a component class (its constructor's argument); a class typed only as
 * `ComponentClass` takes any record.
 */
export type PropsOf<C extends ComponentClass> = C extends new (props: infer P) => Component<object>
  ? [P] extends [never]
    ? Record<string, unknown>
    : P
  : never;

const componentOwners = new WeakMap<object, Owner>();

/**
 * The base class of components: `static template` names the compiled template, `static components`
 * the child classes the template uses, `setup()` runs once before the first render with the component
 * current, so hooks can be registered.
 */
export class Component<P extends object = Record<string, unknown>> {
  /** Current environment, including setup-scoped overrides. Available after construction. */
  get env(): Env {
    const owner = componentOwners.get(this);
    if (owner === undefined) throw new ExtensionError("hook-usage", "Component.env is available after construction");
    return owner.env;
  }
  static template?: string;
  static components?: Record<string, ComponentClass>;
  /** Read-only, also its keys (type review M2): props change only through the parent's bindings. */
  readonly props: Readonly<P>;

  constructor(props: P) {
    this.props = props;
  }

  /** Runs once before the first render; must be synchronous (async work goes in `onWillStart`). */
  setup(): SyncResult {}
}

class ComponentScope extends MountScope implements ComponentState {
  instance!: Component<object>;
  errorHandlers: Array<(error: unknown) => void> | null = null;
  willUnmount: Hook[] | null = null;
  willDestroy: Hook[] | null = null;
  willStart: Array<(this: object) => unknown> | null = null;
  mountedHooks: Hook[] | null = null;
  willUpdateProps: Array<(this: object, nextProps: object) => SyncResult | PromiseLike<unknown>> | null = null;
  flushedHooks: Hook[] | null = null;

  constructor(readonly owner: Owner) {
    super();
  }

  get scope(): MountScope {
    return this;
  }

  override runMounted(): void {
    const hooks = this.mountedHooks;
    if (hooks === null) return;
    this.mountedHooks = null;
    for (let i = 0; i < hooks.length; i++) {
      if (this.owner.disposed) return;
      const prev = enterCallback();
      try {
        routeResult(hooks[i]!.call(this.instance), this.owner);
      } catch (e) {
        handleError(this.owner, e);
      } finally {
        leaveCallback(prev);
      }
    }
  }
}

// ---------------------------------------------------------------------------------------------------
// hooks

let setupTarget: ComponentScope | null = null;

function hookTarget(hook: string): ComponentScope {
  if (setupTarget === null) throw new UIError(`${hook} can only be called during setup()`, "usage");
  return setupTarget;
}

function extensionOwner(hook: string): Owner {
  if (setupTarget === null) throw new ExtensionError("hook-usage", `${hook} can only be called during setup()`);
  return setupTarget.owner;
}

/** Read the environment during component setup. */
export function useEnv(): Env { return extensionOwner("useEnv").env; }
/** Shallow-frozen scope shared by this component and descendants, isolated from siblings. */
export function useSubEnv(extension: Partial<Env>): void {
  const owner = extensionOwner("useSubEnv");
  validateEnvOptions(extension);
  const descriptors: PropertyDescriptorMap = Object.getOwnPropertyDescriptors(extension);
  // Optional undefined fields inherit; materialize standard getters so freezing keeps their types stable.
  for (const key of ["services", "bus", "debug", "isSmall", "context"] as const) {
    const desc = descriptors[key];
    if (desc === undefined) continue;
    const value = extension[key];
    if (value === undefined) delete descriptors[key];
    else descriptors[key] = { value, enumerable: desc.enumerable, configurable: true, writable: false };
  }
  const env = Object.create(owner.env, descriptors) as Env;
  validateEnv(env);
  owner.env = Object.freeze(env);
}
/** Resolve a typed service with owner-bound async delivery protection. */
export function useService<T extends object>(key: ServiceClass<T>): T;
export function useService<K extends keyof Services>(key: K): Services[K];
export function useService(key: ServiceClass | keyof Services): object {
  const owner = extensionOwner("useService");
  return owner.env.services.facade(key, owner);
}

// The hooks are generic in the component type: `onMounted(function (this: MyComp) { … })` sees its
// typed props (type review L2); without an annotation `this` is `Component<object>`.

/**
 * Runs before the first render, with the component as `this`; a returned promise holds the component
 * (and its boundary) until it settles.
 */
export function onWillStart<T extends Component<object> = Component<object>>(
  fn: (this: T) => void | PromiseLike<unknown>,
): void {
  (hookTarget("onWillStart").willStart ??= []).push(fn as (this: object) => unknown);
}

/**
 * Runs once the component's DOM is in the document, children before parents. Synchronous:
 * a promise it returns anyway (plain JS) has its rejection routed to the component's error chain.
 */
export function onMounted<T extends Component<object> = Component<object>>(fn: (this: T) => SyncResult): void {
  (hookTarget("onMounted").mountedHooks ??= []).push(fn as Hook);
}

/** Runs before the component's DOM leaves the document, in reverse registration order. */
export function onWillUnmount<T extends Component<object> = Component<object>>(fn: (this: T) => SyncResult): void {
  (hookTarget("onWillUnmount").willUnmount ??= []).push(fn as Hook);
}

/** Runs when the component is destroyed, in reverse registration order, children first. */
export function onWillDestroy<T extends Component<object> = Component<object>>(fn: (this: T) => SyncResult): void {
  (hookTarget("onWillDestroy").willDestroy ??= []).push(fn as Hook);
}

/**
 * Handles errors of the component's subtree: handlers run in reverse registration order
 * with the component as `this`; returning handles the error, throwing replaces it and passes it up. A
 * handler must be synchronous: one that returns a promise has not handled the error (it could still
 * rethrow), so the runtime passes a `UIError` (code `usage`, the error as its `cause`) up the chain and
 * reports the promise's rejection (review round 2, silent-failure 3, type review H2).
 */
export function onError<T extends Component<object> = Component<object>>(
  fn: (this: T, error: unknown) => SyncResult,
): void {
  const cs = hookTarget("onError");
  (cs.errorHandlers ??= []).push((error) => {
    const result: unknown = (fn as (this: object, error: unknown) => unknown).call(cs.instance, error);
    if (isThenable(result)) {
      result.then(undefined, (e: unknown) => {
        cs.owner.app.report(e);
      });
      throw new UIError(
        `an onError handler of ${cs.instance.constructor.name} returned a promise: onError handlers must be synchronous (the error is this error's cause)`,
        "usage",
        { cause: error },
      );
    }
  });
}

/**
 * An effect owned by the component, disposed with it (OWL 3 `useEffect`); `fn` may return a cleanup. It is
 * kept even when it reads no signal (a listener added in it and removed by its cleanup stays until the
 * component is destroyed).
 */
export function useEffect(fn: () => EffectResult): void {
  const cs = hookTarget("useEffect");
  if (typeof fn !== "function") throw new UIError(`useEffect takes a function, got ${typeof fn}`, "handler");
  createEffect(fn, cs.owner, KEEP);
}

/**
 * Runs synchronously, once per props update, with the complete incoming props before any prop signal is
 * written, so `this.props` still holds the current values. Returned promises hold the
 * candidate until every hook resolves. Newer candidates supersede older ones; unmount cancels commits.
 */
export function onWillUpdateProps<P extends object = Record<string, unknown>, T extends Component<P> = Component<P>>(
  fn: (this: T, nextProps: P) => SyncResult | PromiseLike<unknown>,
): void {
  (hookTarget("onWillUpdateProps").willUpdateProps ??= []).push(fn as (this: object, nextProps: object) => SyncResult | PromiseLike<unknown>);
}

const registeredFlushedComponents = new Set<ComponentScope>();

/** Whether `owner` is `ancestor` or one of its descendants. */
function within(owner: Owner, ancestor: Owner): boolean {
  for (let cur: Owner | null = owner; cur !== null; cur = cur.parent) {
    if (cur === ancestor) return true;
  }
  return false;
}

onFlushComplete((dirtiedOwners) => {
  for (const cs of [...registeredFlushedComponents]) {
    if (cs.owner.disposed || !cs.rendered || cs.flushedHooks === null) continue;
    let touched = false;
    for (const owner of dirtiedOwners) {
      if (within(owner, cs.owner)) {
        touched = true;
        break;
      }
    }
    if (!touched) continue;
    for (const fn of cs.flushedHooks) {
      if (cs.owner.disposed) break;
      const prev = enterCallback();
      try {
        // A promise it returns anyway (plain JS) has its rejection routed (review round 3, R18).
        routeResult(fn.call(cs.instance), cs.owner);
      } catch (e) {
        handleError(cs.owner, e);
      } finally {
        leaveCallback(prev);
      }
    }
  }
});

/**
 * Runs after a flush in which effects of this component's subtree ran, once their DOM writes are done
 * (OWL's `onPatched`): not after the first render, which `onMounted` covers. Signals it
 * writes are applied in the same flush. Synchronous: a promise it returns anyway (plain JS) has its
 * rejection routed to the component's error chain.
 */
export function onFlushed<T extends Component<object> = Component<object>>(fn: (this: T) => SyncResult): void {
  const cs = hookTarget("onFlushed");
  (cs.flushedHooks ??= []).push(fn as Hook);
  registeredFlushedComponents.add(cs);
  cs.owner.onCleanup(() => {
    registeredFlushedComponents.delete(cs);
  });
}

// ---------------------------------------------------------------------------------------------------
// props

/** What the props object of one component needs to know about it: its schema and its scope once created. */
interface PropsContext {
  readonly name: string;
  readonly schema: PropsSchema | null;
  /** The values of the schema's default factories, computed once at creation. */
  readonly defaults: Readonly<Record<string, unknown>>;
  /** The component, once created: its `onWillUpdateProps` hooks run before each update. */
  scope: ComponentScope | null;
}

const hasOwn = (o: object, key: string): boolean => Object.prototype.hasOwnProperty.call(o, key);

/**
 * The props object: one signal per key behind an enumerable getter. Each prop thunk has a
 * binding effect in the parent's owner (a constant prop's effect disposes itself after one run); a
 * `t-props` spread merges under the explicit props, and keys a later spread value no longer has are
 * deleted, or fall back to their schema default. A key the schema gives a default and
 * nothing passes holds the default. With a spread, reads of absent keys and key listings track the
 * shape, so a key that appears later updates its readers.
 *
 * Every update after the first values (a thunk's or the spread's) runs untracked, so what the update
 * reads, the hooks included, never becomes a dependency of a binding (review round 3, R5). It is
 * validated against the schema before anything is written (R7): an invalid update throws
 * into the parent's error chain and the child keeps its props. When the component has
 * `onWillUpdateProps` hooks, the update evaluates every prop, so simultaneous changes reach the hooks
 * as one complete `nextProps` (slots and default-only props included, R20) and are written together;
 * spread changes go through the same path (R6).
 */
function buildProps(
  parentOwner: Owner,
  thunks: Record<string, () => unknown>,
  spread: (() => object) | null,
  slots: Record<string, Slot> | null,
  ctx: PropsContext,
): object {
  const target: Record<string, unknown> = {};
  const signals = new Map<string, CoreSignal<unknown>>();
  const defaults = ctx.defaults;
  const thunkKeys = Object.keys(thunks);
  const shape = spread === null ? null : coreSignal(0);
  /** The current spread keys (not shadowed by a thunk) and their values, defaults applied. */
  let spreadNow: Record<string, unknown> = {};
  // Identity, rather than a numeric generation, also avoids wraparound on long-lived components.
  let candidate: { token: object; next: Record<string, unknown>; spread: Record<string, unknown>; failed: boolean } | null = null;

  // Input evaluation can fail before commit() sees a complete snapshot. A caught parent error must
  // still supersede an older hold, while preserving the original exception and successful deduplication.
  const readInput = <T>(read: () => T): T => {
    try {
      return read();
    } catch (error) {
      candidate = null;
      throw error;
    }
  };

  const sameSnapshot = (a: Record<string, unknown>, b: Record<string, unknown>): boolean => {
    const keys = Object.keys(a);
    return keys.length === Object.keys(b).length && keys.every(k => hasOwn(b, k) && Object.is(a[k], b[k]));
  };

  const withDefault = (key: string, value: unknown): unknown =>
    value === undefined && hasOwn(defaults, key) ? defaults[key] : value;
  const define = (key: string, value: unknown): void => {
    const s = coreSignal<unknown>(value);
    signals.set(key, s);
    Object.defineProperty(target, key, { get: s as () => unknown, enumerable: true, configurable: true });
  };

  /** The props after an update: every thunk's value (fresh or current), the spread, defaults, slots. */
  const snapshot = (key: string | null, value: unknown, spreadNext: Record<string, unknown>, fresh: boolean): Record<string, unknown> => {
    const next: Record<string, unknown> = {};
    for (const k of thunkKeys) {
      next[k] = k === key ? value : fresh ? readInput(() => withDefault(k, thunks[k]!())) : signals.get(k)!();
    }
    for (const k of Object.keys(spreadNext)) next[k] = spreadNext[k];
    for (const k of Object.keys(defaults)) if (!hasOwn(next, k)) next[k] = defaults[k];
    if (slots !== null) next["slots"] = slots;
    return next;
  };

  /** Writes `next` into the signals in one batch, adding and deleting spread keys. */
  const apply = (next: Record<string, unknown>, spreadNext: Record<string, unknown>): void => {
    let shapeChanged = false;
    batch(() => {
      for (const k of Object.keys(next)) {
        if (k === "slots" && slots !== null) continue;
        const s = signals.get(k);
        if (s === undefined) {
          define(k, next[k]);
          shapeChanged = true;
        } else {
          s(next[k]);
        }
      }
      for (const k of [...signals.keys()]) {
        if (hasOwn(next, k)) continue;
        signals.delete(k);
        delete target[k];
        shapeChanged = true;
      }
    });
    spreadNow = spreadNext;
    if (shapeChanged && shape !== null) shape(untrack(() => shape()) + 1);
  };

  /** Whether `next` differs from the props now. */
  const differs = (next: Record<string, unknown>): boolean => {
    for (const k of Object.keys(next)) {
      if (k === "slots" && slots !== null) continue;
      const s = signals.get(k);
      if (s === undefined || !Object.is(s(), next[k])) return true;
    }
    for (const k of signals.keys()) if (!hasOwn(next, k)) return true;
    return false;
  };

  const finish = (token: object): void => {
    const held = candidate;
    if (held === null || held.token !== token || held.failed) return;
    candidate = null;
    if (ctx.scope?.owner.disposed) return;
    apply(held.next, held.spread);
  };
  const abandon = (token: object): boolean => {
    // Keep the failed snapshot for deduplication of sibling bindings in this same input update.
    if (candidate?.token !== token) return false;
    candidate.failed = true;
    return true;
  };

  /** Validates `next`, runs the hooks, then writes it (an update after the first values). */
  const commit = (next: Record<string, unknown>, spreadNext: Record<string, unknown>): void => {
    // Separate bindings can produce the same complete snapshot during one flush. A held update must
    // not restart its hooks for each of those bindings.
    if (candidate !== null && sameSnapshot(candidate.next, next)) return;
    candidate = null;
    // Invalidate BEFORE equality/validation: a reversion or invalid newer input cancels old work too.
    if (!differs(next)) {
      spreadNow = spreadNext;
      return;
    }
    if (ctx.schema !== null) validateProps(ctx.name, next, ctx.schema);
    const cs = ctx.scope;
    if (cs === null || cs.willUpdateProps === null) {
      apply(next, spreadNext);
      return;
    }
    if (cs.owner.disposed) return;
    const token = {};
    candidate = { token, next: Object.freeze(next), spread: spreadNext, failed: false };
    // Stale promise handlers capture only a token, not their obsolete props/spread snapshot.
    runWillUpdateProps(cs, next, () => finish(token), () => abandon(token));
  };

  /** A thunk's new value: a lone write when no hook needs the whole update, else the full path. */
  const update = (key: string, value: unknown): void => {
    const cs = ctx.scope;
    if (cs === null || cs.willUpdateProps === null || cs.owner.disposed) {
      const s = signals.get(key)!;
      if (Object.is(s(), value)) return;
      if (ctx.schema !== null) validateProp(ctx.name, ctx.schema, key, value);
      s(value);
      return;
    }
    const spreadNext = spread === null ? spreadNow : readInput(() => readSpread(spread()));
    commit(snapshot(key, value, spreadNext, true), spreadNext);
  };

  /** The spread's keys (not `slots`, not shadowed by a thunk) and values, defaults applied. */
  const readSpread = (value: unknown): Record<string, unknown> => {
    if (value !== null && value !== undefined && typeof value !== "object") {
      throw new UIError(`t-props takes an object, got ${typeof value}`, "value");
    }
    const out: Record<string, unknown> = {};
    if (value === null || value === undefined) return out;
    const obj = value as Record<string, unknown>;
    for (const key of Object.keys(obj)) {
      // `slots` is the slot record, never data: a spread never sets it (security review L2).
      if (key === "slots" || hasOwn(thunks, key)) continue;
      out[key] = withDefault(key, obj[key]);
    }
    return out;
  };

  for (const key of thunkKeys) {
    if (key === "slots" && slots !== null) {
      throw new UIError(`a prop named "slots" cannot be passed together with slot content`, "value");
    }
    define(key, undefined);
    const s = signals.get(key)!;
    const thunk = thunks[key]!;
    let initial = true;
    createEffect(() => {
      const value = readInput(() => withDefault(key, thunk()));
      if (initial) {
        initial = false;
        s(value);
        return;
      }
      untrack(() => update(key, value));
    }, parentOwner);
  }
  for (const key of Object.keys(defaults)) {
    if (!hasOwn(thunks, key) && !(key === "slots" && slots !== null)) define(key, defaults[key]);
  }
  if (slots !== null) {
    Object.defineProperty(target, "slots", { value: slots, enumerable: true, configurable: false });
  }
  if (spread === null) return target;

  let initialSpread = true;
  createEffect(() => {
    // Read tracked: a store spread value re-runs this when its keys or values change.
    const spreadNext = readInput(() => readSpread(spread()));
    untrack(() => {
      if (initialSpread) {
        initialSpread = false;
        apply(snapshot(null, undefined, spreadNext, false), spreadNext);
        return;
      }
      const cs = ctx.scope;
      const fresh = cs !== null && cs.willUpdateProps !== null && !cs.owner.disposed;
      commit(snapshot(null, undefined, spreadNext, fresh), spreadNext);
    });
  }, parentOwner);
  return new Proxy(target, {
    get(t, key, receiver) {
      if (typeof key === "string" && !(key in t)) shape!();
      return Reflect.get(t, key, receiver);
    },
    has(t, key) {
      shape!();
      return Reflect.has(t, key);
    },
    ownKeys(t) {
      shape!();
      return Reflect.ownKeys(t);
    },
  });
}

/**
 * Starts every hook without serializing async work. Failure refuses the entire candidate, while
 * every returned thenable has a rejection handler, including obsolete candidates and disposed owners.
 * Synchronous-only updates still commit synchronously.
 */
function runWillUpdateProps(cs: ComponentScope, next: Record<string, unknown>, commit: () => void, abandon: () => boolean): void {
  let failed = false;
  const pending: Promise<void>[] = [];
  const fail = (error: unknown): void => {
    failed = true;
    // Obsolete work is observable through reporting, but must not replace a newer successful view
    // with a boundary fallback. A live candidate uses the normal owner error chain.
    if (abandon()) routeError(cs.owner, error);
    else cs.owner.app.report(error);
  };
  for (const fn of cs.willUpdateProps!) {
    if (cs.owner.disposed) break;
    const prev = enterCallback();
    try {
      const result: unknown = fn.call(cs.instance, next);
      if (isThenable(result)) {
        pending.push(Promise.resolve(trackTestWork(result)).then(() => {}, fail));
      }
    } catch (e) {
      fail(e);
    } finally {
      leaveCallback(prev);
    }
  }
  if (pending.length === 0) {
    if (!failed) commit();
  } else {
    void Promise.all(pending).then(() => {
      if (!failed) commit();
    }).catch(error => routeError(cs.owner, error));
  }
}

// ---------------------------------------------------------------------------------------------------
// creating and rendering

function lookup(self: object, name: string): ComponentClass {
  const parentClass = (self as { constructor: ComponentClass }).constructor;
  const record = parentClass.components;
  if (record !== undefined && Object.prototype.hasOwnProperty.call(record, name)) {
    const C = record[name];
    if (typeof C === "function") return C;
  }
  throw new UIError(`Cannot find the definition of component "${name}" in ${parentClass.name}.components`, "template");
}

function templateOf(app: AppContext, C: ComponentClass): (self: object) => Node | null {
  const name = C.template;
  if (typeof name !== "string" || name === "") {
    throw new UIError(`component ${C.name} has no static template`, "template");
  }
  const fn = app.template(name);
  if (fn === undefined) throw new UIError(`template "${name}" of component ${C.name} is not registered`, "template");
  return fn;
}

/**
 * Creates a component at a position (§7.5): props, instance, `setup()`, `onWillStart`, then the first
 * render, now or once every `onWillStart` promise has resolved.
 */
function createComponent(
  C: ComponentClass,
  parentOwner: Owner,
  parent: Node,
  end: Node | null,
  thunks: Record<string, () => unknown>,
  spread: (() => object) | null,
  slots: Record<string, Slot> | null,
): ComponentScope {
  const declared = (C as unknown as { props?: unknown }).props;
  const schema = isPropsSchema(declared) ? declared : null;
  const defaults: Record<string, unknown> = {};
  if (schema !== null) {
    for (const [key, type] of Object.entries(schema.shape)) {
      if (type.defaultFactory !== undefined) defaults[key] = type.defaultFactory();
    }
  }
  const ctx: PropsContext = { name: C.name, schema, defaults, scope: null };
  const props = buildProps(parentOwner, thunks, spread, slots, ctx);
  const owner = parentOwner.child();
  const cs = new ComponentScope(owner);
  ctx.scope = cs;
  owner.comp = cs;
  const registeredIn = parentOwner.scope;
  if (registeredIn.mounted || registeredIn.kids === null) {
    throw new UIError(`internal: component ${C.name} created outside a build (its mount scope has already mounted)`, "usage");
  }
  registeredIn.kids.push(cs);
  owner.scope = cs;
  parentOwner.onCleanup(() => dispose(owner));

  const prevOwner = setOwner(owner);
  const prevSetup = setupTarget;
  setupTarget = cs;
  try {
    // The props already hold their defaults (buildProps); this checks them, never rewrites them, so
    // every prop keeps its binding (review round 3, R1).
    if (schema !== null) validateProps(C.name, untrack(() => ({ ...props }) as Record<string, unknown>), schema);
    const instance = new (C as unknown as new (props: object) => Component<object>)(props);
    cs.instance = instance;
    componentOwners.set(instance, owner);
    const result: unknown = instance.setup();
    if (isThenable(result)) {
      // Hooks registered after its first await would land outside setup(): refused, and the promise's
      // own rejection is reported (review round 2, silent-failure 3, type review H2).
      result.then(undefined, (e: unknown) => {
        owner.app.report(e);
      });
      throw new UIError(`setup() of ${C.name} must be synchronous; move async work to onWillStart`, "usage");
    }
  } catch (e) {
    // OWL builds the component inside the parent's render, so an error of the constructor or `setup()`
    // starts at the parent's handlers, not at handlers the failing setup registered (review L10).
    cs.rendered = false;
    handleError(parentOwner, e);
    return cs;
  } finally {
    setupTarget = prevSetup;
    setOwner(prevOwner);
  }

  const willStart = cs.willStart;
  cs.willStart = null;
  let promises: PromiseLike<unknown>[] | null = null;
  if (willStart !== null) {
    for (const fn of willStart) {
      const prev = enterCallback();
      try {
        const r = fn.call(cs.instance);
        if (isThenable(r)) (promises ??= []).push(trackTestWork(r));
      } catch (e) {
        // The promises of earlier hooks are abandoned with the component; a rejection of theirs is
        // reported, never dropped (review L3, review round 2 silent-failure 5).
        if (promises !== null) for (const p of promises) reportRejection(p, owner);
        cs.rendered = false;
        handleError(owner, e);
        return cs;
      } finally {
        leaveCallback(prev);
      }
    }
  }
  if (promises === null) {
    render(C, cs, parent, end);
    return cs;
  }
  cs.rendered = false;
  const boundary = owner.boundary !== null && !owner.boundary.committed ? owner.boundary : null;
  if (boundary !== null) boundary.pending++;
  // Like `Promise.all` (the first rejection settles it at once, as in OWL), but every later rejection is
  // reported too (review round 2, silent-failure 5). Whatever the continuation throws (a detached
  // anchor, a commit that fails) reaches the owner's chain, and the boundary is always released, so a
  // mount never hangs (silent-failure 4).
  let left = promises.length;
  let done = false;
  const finish = (failed: boolean, error: unknown): void => {
    done = true;
    try {
      if (failed) routeError(owner, error);
      else if (!owner.disposed) {
        render(C, cs, parent, end);
        if (boundary === null && registeredIn.mounted) mountTree(cs);
      }
    } catch (e) {
      routeError(owner, e);
    } finally {
      if (boundary !== null) releaseBoundary(boundary, owner);
    }
  };
  for (const p of promises) {
    Promise.resolve(p).then(
      () => {
        if (--left === 0 && !done) finish(false, undefined);
      },
      (error: unknown) => {
        if (done) owner.app.report(error);
        else finish(true, error);
      },
    );
  }
  return cs;
}

/** Reports a rejection of a promise whose component is already failing for another reason. */
function reportRejection(p: PromiseLike<unknown>, owner: Owner): void {
  Promise.resolve(p).then(undefined, (e: unknown) => {
    owner.app.report(e);
  });
}

/** Releases `boundary`; an error of the commit it starts goes to the owner's chain or the app's report. */
function releaseBoundary(boundary: Boundary, owner: Owner): void {
  try {
    boundary.release();
  } catch (e) {
    routeError(owner, e);
  }
}

function render(C: ComponentClass, cs: ComponentScope, parent: Node, end: Node | null): void {
  const owner = cs.owner;
  const builtin = (C as BuiltinClass)[RENDER];
  const prev = setOwner(owner);
  try {
    if (builtin !== undefined) {
      builtin(cs, parent, end);
    } else {
      const fn = templateOf(owner.app, C);
      const node = untrack(() => fn(cs.instance));
      // Inside the try: an anchor code outside the runtime removed fails here, into the chain (review
      // round 2, silent-failure 4). A build that disposed its own owner inserts nothing.
      if (node !== null && !owner.disposed) insertBefore(parent, end, node);
    }
  } catch (e) {
    handleError(owner, e);
  } finally {
    setOwner(prev);
  }
  if (owner.disposed) return;
  cs.rendered = true;
}

/**
 * `<Child …/>` in a template (§7.5): the class is looked up in `self.constructor.components[name]`.
 */
export function component(
  parent: Node,
  marker: Node | null,
  self: object,
  name: string,
  props: Record<string, () => unknown>,
  spread: (() => object) | null,
  slots: Record<string, Slot> | null,
): void {
  const owner = requireOwner("component");
  const C = lookup(self, name);
  createComponent(C, owner, parent, endAnchor(parent, marker), props, spread, slots);
}

function slotOf(self: object, name: string): Slot | undefined {
  const slots = (self as { props?: { slots?: Record<string, Slot> } }).props?.slots;
  if (slots === undefined || slots === null || !Object.prototype.hasOwnProperty.call(slots, name)) return undefined;
  const s = slots[name];
  if (s === undefined) return undefined;
  if (s === null || typeof s !== "object" || typeof s.render !== "function") {
    throw new UIError(`slot "${name}" is not a slot ({render})`, "template");
  }
  return s;
}

/**
 * `t-call-slot` (§7.2,/046/047): renders `self.props.slots[name].render(scope())` in a child
 * owner, rebuilding when `scope()` returns another object; a missing slot renders the fallback, or
 * nothing. Slot content belongs to the caller's template but lives as long as this position.
 */
export function slot(
  parent: Node,
  marker: Node | null,
  self: object,
  name: string,
  scope: (() => object) | null,
  fallback: (() => Node | null) | null,
): void {
  const owner = requireOwner("slot");
  const end = endAnchor(parent, marker);
  const s = slotOf(self, name);
  const region = new Region(parent, end);
  owner.onCleanup(() => region.dispose());
  if (s === undefined) {
    if (fallback !== null) region.build(fallback, owner);
    return;
  }
  if (scope === null) {
    region.build(() => s.render(), owner);
    return;
  }
  let built = false;
  let current: object | undefined;
  createEffect(() => {
    const value = scope();
    if (built && Object.is(value, current)) return;
    built = true;
    current = value;
    region.clear();
    region.build(() => s.render(value), owner);
  }, owner);
}

// ---------------------------------------------------------------------------------------------------
// built-in boundaries

/**
 * A pending boundary (§7.6): its `default` slot is built out of the document and inserted in one step
 * when every component inside it has finished `onWillStart`; until then its `fallback` slot shows.
 */
export class Pending extends Component<{ slots?: Record<string, Slot> }> {
  static [RENDER](cs: ComponentScope, parent: Node, end: Node | null): void {
    const owner = cs.owner;
    const self = cs.instance;
    const content = owner.child();
    const frag = document.createDocumentFragment();
    const anchor = frag.appendChild(document.createTextNode(""));
    let fallback: Region | null = null;
    const holder: Boundary = new Boundary(() => {
      holder.committed = true;
      holder.rendered = true;
      if (owner.disposed || content.disposed) return;
      // The commit runs from release(), often in a promise continuation: an error here (a detached
      // anchor) goes to the boundary's chain, never to an unhandled rejection (review round 2, 4).
      try {
        if (fallback !== null) {
          const f = fallback;
          fallback = null;
          f.clear();
        }
        insertBefore(parent, end, frag);
      } catch (e) {
        routeError(owner, e);
        return;
      }
      if (cs.mounted) mountTree(holder);
      else cs.kids?.push(holder);
    });
    content.boundary = holder;
    content.scope = holder;
    owner.onCleanup(() => {
      fallback?.dispose();
      dispose(content);
    });
    const def = slotOf(self, "default");
    const prev = setOwner(content);
    let node: Node | null = null;
    try {
      if (def !== undefined) node = untrack(() => def.render());
    } catch (e) {
      handleError(content, e);
    } finally {
      setOwner(prev);
    }
    if (content.disposed) return;
    if (node !== null) frag.insertBefore(node, anchor);
    if (holder.pending === 0) {
      holder.tryCommit();
      return;
    }
    const fb = slotOf(self, "fallback");
    if (fb !== undefined) {
      fallback = new Region(parent, end);
      fallback.build(() => fb.render(), owner);
    }
  }
}

/**
 * An error boundary: an error of its `default` slot's subtree switches it to its
 * `fallback` slot (scope `{error, reset}`) or, without one, to a minimal panel with a Retry button. The
 * failing subtree is disposed at once, before any of its queued effects can run. An error of the
 * fallback itself goes to the boundary's parents.
 */
export class ErrorBoundary extends Component<{ slots?: Record<string, Slot> }> {
  static [RENDER](cs: ComponentScope, parent: Node, end: Node | null): void {
    const owner = cs.owner;
    const self = cs.instance;
    const region = new Region(parent, end);
    let showingFallback = false;
    let building = false;
    let deferred: { error: unknown } | null = null;
    owner.onCleanup(() => region.dispose());

    const reset = (): void => {
      if (!showingFallback || owner.disposed) return;
      region.clear();
      showingFallback = false;
      buildContent();
    };
    const showFallback = (error: unknown): void => {
      region.clear();
      showingFallback = true;
      const fb = slotOf(self, "fallback");
      region.build(() => (fb !== undefined ? fb.render({ error, reset }) : errorPanel(owner, reset)), owner);
    };
    const buildContent = (): void => {
      const def = slotOf(self, "default");
      building = true;
      let scope: MountScope | null = null;
      try {
        scope = region.buildHeld(() => (def !== undefined ? def.render() : null), owner);
      } finally {
        building = false;
      }
      if (deferred !== null) {
        // Content that failed while it was built never mounts: it is replaced before any of its
        // `onMounted` hooks run (review M4; OWL never mounts a failed render either).
        const { error } = deferred;
        deferred = null;
        showFallback(error);
        return;
      }
      endBuild(scope);
    };
    (cs.errorHandlers ??= []).push((error) => {
      // A boundary being disposed never switches to a fallback nobody would see (and whose owner would
      // leak): the error goes on to its parents (review round 2, silent-failure 2).
      if (showingFallback || owner.disposed) throw error;
      if (building) {
        // The first error of a build switches to the fallback; later ones of the same build are
        // reported, never dropped (review M4).
        if (deferred === null) deferred = { error };
        else owner.app.report(error);
        return;
      }
      showFallback(error);
    });
    buildContent();
  }
}

function errorPanel(owner: Owner, reset: () => void): Node {
  const panel = document.createElement("div");
  panel.setAttribute("role", "alert");
  panel.className = "o_error_boundary";
  const p = panel.appendChild(document.createElement("p"));
  p.textContent = "This part of the page could not be displayed.";
  const button = panel.appendChild(document.createElement("button"));
  button.type = "button";
  button.textContent = "Retry";
  button.addEventListener("click", () => {
    try {
      reset();
    } catch (e) {
      handleError(owner, e);
    }
  });
  return panel;
}

/** The element a Portal's `target` prop names: the element itself, or the first match of a selector. */
function portalTarget(target: unknown): Element {
  let element: Element | null = null;
  if (typeof target === "string") {
    try {
      element = document.querySelector(target);
    } catch (e) {
      throw new UIError(`Portal target is not a valid selector: ${target}`, "usage", { cause: e });
    }
  }
  else if (target instanceof Element) element = target;
  if (element === null) throw new UIError(`Portal target not found: ${String(target)}`, "usage");
  return element;
}

/**
 * A portal component: mounts its content into an external DOM element
 * while preserving the component owner hierarchy for error propagation and lifecycle cleanup. The
 * content sits between two anchors appended to the target; when `target` changes, the anchors and the
 * content move to the new target without being rebuilt (review round 3, R15). A target that cannot be
 * found, at mount or later, is a `UIError` (code `usage`) in the portal's error chain, and the content
 * stays where it is.
 */
export class Portal extends Component<{ target: Element | string; slots?: Record<string, Slot> }> {
  static [RENDER](cs: ComponentScope, _parent: Node, _end: Node | null): void {
    const owner = cs.owner;
    const self = cs.instance;
    const props = self.props as { target: Element | string; slots?: Record<string, Slot> };
    const start = document.createTextNode("");
    const end = document.createTextNode("");
    let current: Element | null = null;
    let region: Region | null = null;
    owner.onCleanup(() => {
      region?.clear();
      start.remove();
      end.remove();
    });
    createEffect(() => {
      const element = portalTarget(props.target);
      if (current === null) {
        current = element;
        element.append(start, end);
        region = new Region(element, end);
        const defaultSlot = slotOf(self, "default");
        if (defaultSlot !== undefined) region.build(() => defaultSlot.render(), owner);
        return;
      }
      if (element === current) return;
      // The range is checked before anything moves: code outside the runtime that broke it is an error,
      // never a partial move.
      if (start.parentNode !== current || end.parentNode !== current) throw detached();
      for (let n: Node | null = start; n !== end; n = n.nextSibling) {
        if (n === null) throw detached();
      }
      const moved = document.createDocumentFragment();
      let n: Node = start;
      for (;;) {
        const next = n.nextSibling;
        moved.appendChild(n);
        if (n === end) break;
        n = next!;
      }
      element.appendChild(moved);
      current = element;
    }, owner);
  }
}

/** Fixed-height virtual window options; every geometry value is checked at runtime. */
export interface VirtualProps<T> {
  readonly items: readonly T[];
  readonly getKey: (item: T, index: number) => unknown;
  readonly itemHeight: number;
  readonly viewportHeight: number;
  /** Nonnegative integer, at most 1000; omitted means two rows on each side. */
  readonly overscan?: number;
  readonly slots?: Record<string, Slot>;
}

/** The scoped row slot receives accessors, so kept keys can change item and original index. */
export interface VirtualRow<T> {
  readonly item: () => T;
  readonly index: () => number;
}

/**
 * Fixed-height, keyed array window. Register in `static components` as `Virtual` and provide a
 * default scoped slot receiving `VirtualRow<T>`. Offscreen rows are destroyed; overlapping keys keep
 * their owners. Heights must be positive/finite; overscan is 0..1000; total height is at most
 * 16,000,000 CSS pixels. Invalid geometry is a UIError (`value`), a missing slot (`template`).
 * No paging, variable-height measurement or view-specific grid keyboard behavior is supplied.
 */
export class Virtual<T = unknown> extends Component<VirtualProps<T>> {
  static [RENDER](cs: ComponentScope, parent: Node, end: Node | null): void {
    const owner = cs.owner;
    const self = cs.instance as Virtual;
    const props = self.props;
    const rowSlot = slotOf(self, "default");
    if (rowSlot === undefined) throw new UIError("Virtual requires a default row slot", "template");
    const viewport = document.createElement("div");
    viewport.dataset["elangVirtual"] = "";
    viewport.tabIndex = 0;
    viewport.setAttribute("role", "list");
    viewport.style.overflowY = "auto";
    const body = viewport.appendChild(document.createElement("div"));
    body.style.position = "relative";
    const scroll = coreSignal(0);
    type Entry = { item: unknown; index: number; key: unknown };
    const window = computed(() => {
      if (!Array.isArray(props.items)) throw new UIError("Virtual items must be an array", "value");
      if (typeof props.getKey !== "function") throw new UIError("Virtual getKey must be a function", "value");
      for (const name of ["itemHeight", "viewportHeight"] as const) {
        if (typeof props[name] !== "number" || !Number.isFinite(props[name]) || props[name] <= 0) {
          throw new UIError(`Virtual ${name} must be a finite positive number`, "value");
        }
      }
      // The default applies only to absence; null/wrong types are rejected.
      const overscan = props.overscan === undefined ? 2 : props.overscan;
      if (!Number.isInteger(overscan) || overscan < 0 || overscan > 1000) {
        throw new UIError("Virtual overscan must be an integer from 0 to 1000", "value");
      }
      const totalHeight = props.items.length * props.itemHeight;
      if (!Number.isFinite(totalHeight) || totalHeight > 16_000_000) {
        throw new UIError("Virtual total height must not exceed 16000000 CSS pixels", "value");
      }
      const requested = scroll();
      if (!Number.isFinite(requested)) throw new UIError("Virtual scroll position must be finite", "value");
      const position = Math.max(0, Math.min(requested, Math.max(0, totalHeight - props.viewportHeight)));
      const start = Math.max(0, Math.floor(position / props.itemHeight) - overscan);
      const stop = Math.min(props.items.length, Math.ceil((position + props.viewportHeight) / props.itemHeight) + overscan);
      if (stop - start > 10_000) throw new UIError("Virtual window must not exceed 10000 rows", "value");
      // Validate the entire candidate window before either spacers or rows change. Otherwise a
      // caught duplicate-key error could leave old rows under the new window's spacer geometry.
      const entries: Entry[] = [];
      const keys = new Set<unknown>();
      for (let index = start; index < stop; index++) {
        const item = props.items[index];
        const key = props.getKey(item, index);
        if (keys.has(key)) throw new UIError(`Got duplicate key in t-foreach: ${String(key)}`, "loop");
        keys.add(key);
        entries.push({ item, index, key });
      }
      return { position, totalHeight, items: props.items, entries,
        height: props.itemHeight, viewportHeight: props.viewportHeight };
    });
    const onScroll = (): void => { scroll(viewport.scrollTop); };
    viewport.addEventListener("scroll", onScroll);
    owner.onCleanup(() => viewport.removeEventListener("scroll", onScroll));
    insertBefore(parent, end, viewport);
    createEffect(() => {
      const view = window();
      viewport.style.height = `${view.viewportHeight}px`;
      body.style.height = `${view.totalHeight}px`;
      viewport.scrollTop = view.position;
      if (untrack(() => scroll()) !== view.position) scroll(view.position);
    }, owner);
    list(body, null, () => window().entries, entry => (entry as Entry).key, row => {
      const rowOwner = requireOwner("Virtual row");
      const element = document.createElement("div");
      element.setAttribute("role", "listitem");
      element.style.boxSizing = "border-box";
      element.style.overflow = "hidden";
      element.style.position = "absolute";
      element.style.left = "0";
      element.style.right = "0";
      const entry = (): Entry => row.item() as Entry;
      createEffect(() => {
        element.style.height = `${window().height}px`;
        // Position from the original index rather than summing rounded CSS row heights.
        element.style.top = `${entry().index * window().height}px`;
        element.setAttribute("aria-posinset", String(entry().index + 1));
        element.setAttribute("aria-setsize", String(window().items.length));
      }, rowOwner);
      const scope: VirtualRow<unknown> = { item: () => entry().item, index: () => entry().index };
      const node = rowSlot.render(scope);
      if (node !== null) element.appendChild(node);
      return element;
    });
  }
}

/**
 * Seam through which `VariableList` learns a row's height: call `report(height)` at once and on every size
 * change, return a function that stops observing. The default uses a `ResizeObserver` (border-box height);
 * tests and non-browser hosts inject their own, because jsdom has no layout.
 */
export type VariableListObserve = (element: HTMLElement, report: (height: number) => void) => () => void;

/** Variable-height window options; every geometry value is checked at runtime. */
export interface VariableListProps<T> {
  readonly items: readonly T[];
  /** Keys must be unique across the whole array, not only the window. */
  readonly getKey: (item: T, index: number) => unknown;
  /** Finite positive CSS pixels used for a row until it reports a measured height. */
  readonly estimatedHeight: number;
  readonly viewportHeight: number;
  /** Nonnegative integer, at most 1000; omitted means two rows on each side. */
  readonly overscan?: number;
  /** When true, a viewport that was at the end stays at the end after an update (chat append). */
  readonly followEnd?: boolean;
  /** Height measurement seam; omitted means a `ResizeObserver`. */
  readonly observe?: VariableListObserve;
  /** Accessible name of the focusable list container. */
  readonly ariaLabel?: string;
  readonly slots?: Record<string, Slot>;
}

const VARIABLE_LIST_MAX_ITEMS = 100_000;

function defaultObserve(element: HTMLElement, report: (height: number) => void): () => void {
  const observer = new ResizeObserver(entries => {
    for (const entry of entries) {
      // Safari before 15.4 has no borderBoxSize; the bounding box is the same border-box height.
      const height = entry.borderBoxSize?.[0]?.blockSize ?? entry.target.getBoundingClientRect().height;
      // Height 0 means "not laid out" (display:none tab, folded panel, detached subtree): keep the
      // previous measurement instead of collapsing the row to nothing. Real rows are never 0px tall.
      if (height === 0) continue;
      report(height);
    }
  });
  observer.observe(element, { box: "border-box" });
  return () => observer.disconnect();
}

/** Index of the last offset that is `<= y`, clamped to `0..count-1` (0 for an empty list). */
function rowAt(offsets: Float64Array, count: number, y: number): number {
  let low = 0, high = count - 1;
  while (low < high) {
    const mid = (low + high + 1) >> 1;
    if (offsets[mid]! <= y) low = mid; else high = mid - 1;
  }
  return Math.max(0, low);
}

/**
 * Variable-height, keyed array window for lists whose rows differ in height and whose older rows are
 * prepended (chat, threads). Register in `static components` as `VariableList` and provide a default
 * scoped slot receiving `VirtualRow<T>`. Unmeasured rows use `estimatedHeight`; each rendered row is
 * observed (see `VariableListObserve`) and its measured height is kept per key. When the items or a
 * measured height change, the first visible row (by key) keeps its place in the viewport, so prepending
 * older rows does not move what the user reads; `followEnd` keeps an at-the-end viewport at the end.
 * Limits: at most 100,000 items, total height 16,000,000 CSS pixels, a window of 10,000 rows, overscan
 * 0..1000. Invalid geometry or seams are UIError `value`, duplicate keys `loop`, a missing slot `template`.
 * Row margins are not measured; rows are absolutely positioned at their offset.
 */
export class VariableList<T = unknown> extends Component<VariableListProps<T>> {
  static [RENDER](cs: ComponentScope, parent: Node, end: Node | null): void {
    const owner = cs.owner;
    const self = cs.instance as VariableList;
    const props = self.props;
    const rowSlot = slotOf(self, "default");
    if (rowSlot === undefined) throw new UIError("VariableList requires a default row slot", "template");
    const viewport = document.createElement("div");
    viewport.dataset["elangVariableList"] = "";
    viewport.tabIndex = 0;
    viewport.setAttribute("role", "list");
    viewport.style.overflowY = "auto";
    // The list does its own anchoring; the browser's scroll anchoring would shift the position twice.
    viewport.style.setProperty("overflow-anchor", "none");
    const body = viewport.appendChild(document.createElement("div"));
    body.style.position = "relative";
    const scroll = coreSignal(0);
    const heightsVersion = coreSignal(0);
    const measured = new Map<unknown, number>();
    type Entry = { item: unknown; index: number; key: unknown };
    // `seenItems` and `previous` are memory of the computeds below, written inside them. They are
    // deterministic functions of the last inputs: recomputing with the same inputs gives the same result.
    let seenItems: readonly unknown[] | null = null;
    const layout = computed(() => {
      if (!Array.isArray(props.items)) throw new UIError("VariableList items must be an array", "value");
      if (typeof props.getKey !== "function") throw new UIError("VariableList getKey must be a function", "value");
      if (typeof props.estimatedHeight !== "number" || !Number.isFinite(props.estimatedHeight) || props.estimatedHeight <= 0) {
        throw new UIError("VariableList estimatedHeight must be a finite positive number", "value");
      }
      if (props.items.length > VARIABLE_LIST_MAX_ITEMS) {
        throw new UIError("VariableList must not have more than 100000 items", "value");
      }
      heightsVersion();
      const items = props.items as readonly unknown[];
      const count = items.length;
      const keys: unknown[] = new Array(count);
      const index = new Map<unknown, number>();
      const offsets = new Float64Array(count + 1);
      let total = 0;
      for (let i = 0; i < count; i++) {
        const key = props.getKey(items[i], i);
        if (index.has(key)) throw new UIError(`Got duplicate key in t-foreach: ${String(key)}`, "loop");
        index.set(key, i);
        keys[i] = key;
        offsets[i] = total;
        total += measured.get(key) ?? props.estimatedHeight;
      }
      offsets[count] = total;
      if (!Number.isFinite(total) || total > 16_000_000) {
        throw new UIError("VariableList total height must not exceed 16000000 CSS pixels", "value");
      }
      if (items !== seenItems) {
        seenItems = items;
        // Bound the cache by the item count: heights of keys that left the array are dropped.
        for (const key of measured.keys()) if (!index.has(key)) measured.delete(key);
      }
      return { items, count, keys, index, offsets, total };
    });
    type Layout = ReturnType<typeof layout>;
    let previous: { layout: Layout; position: number; viewportHeight: number } | null = null;
    const state = computed(() => {
      const lay = layout();
      if (typeof props.viewportHeight !== "number" || !Number.isFinite(props.viewportHeight) || props.viewportHeight <= 0) {
        throw new UIError("VariableList viewportHeight must be a finite positive number", "value");
      }
      const overscan = props.overscan === undefined ? 2 : props.overscan;
      if (!Number.isInteger(overscan) || overscan < 0 || overscan > 1000) {
        throw new UIError("VariableList overscan must be an integer from 0 to 1000", "value");
      }
      if (props.followEnd !== undefined && typeof props.followEnd !== "boolean") {
        throw new UIError("VariableList followEnd must be a boolean", "value");
      }
      if (props.ariaLabel !== undefined && typeof props.ariaLabel !== "string") {
        throw new UIError("VariableList ariaLabel must be a string", "value");
      }
      if (props.observe !== undefined && typeof props.observe !== "function") {
        throw new UIError("VariableList observe must be a function", "value");
      }
      if (props.observe === undefined && typeof ResizeObserver === "undefined") {
        throw new UIError("VariableList needs a ResizeObserver or an observe function", "value");
      }
      const requested = scroll();
      if (!Number.isFinite(requested)) throw new UIError("VariableList scroll position must be finite", "value");
      const viewportHeight = props.viewportHeight;
      const maxPosition = Math.max(0, lay.total - viewportHeight);
      let position = requested;
      if (previous === null) {
        if (props.followEnd === true) position = maxPosition;
      } else {
        const old = previous.layout;
        const wasAtEnd = requested >= old.total - previous.viewportHeight - 0.5;
        const changed = previous.layout !== lay || previous.viewportHeight !== viewportHeight;
        if (props.followEnd === true && wasAtEnd && changed) {
          position = maxPosition;
        } else if (previous.layout !== lay && old.count > 0) {
          // Anchor on the first row at the viewport top: same key, same distance from the top edge.
          const row = rowAt(old.offsets, old.count, requested);
          const moved = lay.index.get(old.keys[row]);
          if (moved !== undefined) {
            position = lay.offsets[moved]! + (requested - old.offsets[row]!);
          } else {
            // The anchor row is gone: use the nearest surviving row. A survivor below takes the removed
            // row's place (its top edge); a survivor above ends where the removed rows began.
            for (let d = 1; d < old.count; d++) {
              const below = row + d < old.count ? lay.index.get(old.keys[row + d]) : undefined;
              if (below !== undefined) { position = lay.offsets[below]!; break; }
              const above = row - d >= 0 ? lay.index.get(old.keys[row - d]) : undefined;
              if (above !== undefined) { position = lay.offsets[above + 1]!; break; }
            }
          }
        }
      }
      position = Math.max(0, Math.min(position, maxPosition));
      const first = rowAt(lay.offsets, lay.count, position);
      let lastRow = rowAt(lay.offsets, lay.count, position + viewportHeight);
      // A row that starts exactly at the viewport's bottom edge is not visible.
      if (lastRow > first && lay.offsets[lastRow]! >= position + viewportHeight) lastRow--;
      const last = lay.count === 0 ? 0 : lastRow + 1;
      const start = Math.max(0, first - overscan);
      const stop = Math.min(lay.count, last + overscan);
      if (stop - start > 10_000) throw new UIError("VariableList window must not exceed 10000 rows", "value");
      const entries: Entry[] = [];
      for (let i = start; i < stop; i++) entries.push({ item: lay.items[i], index: i, key: lay.keys[i] });
      previous = { layout: lay, position, viewportHeight };
      return { position, lay, entries, viewportHeight };
    });
    const onScroll = (): void => { scroll(viewport.scrollTop); };
    viewport.addEventListener("scroll", onScroll);
    owner.onCleanup(() => viewport.removeEventListener("scroll", onScroll));
    insertBefore(parent, end, viewport);
    createEffect(() => {
      const view = state();
      viewport.style.height = `${view.viewportHeight}px`;
      body.style.height = `${view.lay.total}px`;
      if (viewport.scrollTop !== view.position) viewport.scrollTop = view.position;
      if (untrack(() => scroll()) !== view.position) scroll(view.position);
      if (props.ariaLabel === undefined) viewport.removeAttribute("aria-label");
      else viewport.setAttribute("aria-label", props.ariaLabel);
    }, owner);
    list(body, null, () => state().entries, entry => (entry as Entry).key, row => {
      const rowOwner = requireOwner("VariableList row");
      const element = document.createElement("div");
      element.setAttribute("role", "listitem");
      element.style.boxSizing = "border-box";
      element.style.position = "absolute";
      element.style.left = "0";
      element.style.right = "0";
      const entry = (): Entry => row.item() as Entry;
      const key = untrack(() => entry().key);
      createEffect(() => {
        const view = state();
        element.style.top = `${view.lay.offsets[entry().index]}px`;
        element.setAttribute("aria-posinset", String(entry().index + 1));
        element.setAttribute("aria-setsize", String(view.lay.count));
      }, rowOwner);
      const scope: VirtualRow<unknown> = { item: () => entry().item, index: () => entry().index };
      const node = rowSlot.render(scope);
      if (node !== null) element.appendChild(node);
      let stopped = false;
      const stopObserving = (untrack(() => props.observe) ?? defaultObserve)(element, height => {
        // A report after the row was disposed (unmount, left the window) is stale: ignore it.
        if (stopped) return;
        if (typeof height !== "number" || !Number.isFinite(height) || height <= 0) {
          // Reports arrive from observer callbacks, outside any render: route to the owner's error chain
          // (onError handlers, then the app's report channel) instead of throwing into the observer.
          routeError(owner, new UIError("VariableList reported row height must be a finite positive number", "value"));
          return;
        }
        // A report for a key removed before the update flushed is dropped by the next layout's pruning, and
        // a disposed row is `stopped`, so the cache stays bounded by the item count without a membership test.
        if (measured.get(key) === height) return;
        measured.set(key, height);
        heightsVersion(untrack(() => heightsVersion()) + 1);
      });
      rowOwner.onCleanup(() => { stopped = true; stopObserving(); });
      return element;
    });
  }
}

// ---------------------------------------------------------------------------------------------------
// mount

/** The options of `mount` other than the root props. */
export interface MountBase {
  /** Isolated environment; omitted means a fresh environment per app. */
  env?: Env;
  /**
   * The compiled template modules the app's components use. Required (type review L6): every component
   * that is not a built-in needs its template here; a root that is a built-in passes `[]`.
   */
  templates: Iterable<TemplateModule>;
  /**
   * Receives every error no `onError` handler took: the one that made the root show its crash panel
   * (also while the mount promise is pending, which then rejects as well), and errors reported without
   * a crash. Without it, errors after the mount resolved go to `globalThis.reportError`, and an error
   * before that only rejects the mount promise (one channel, review L2).
   */
  onError?: (error: unknown) => void;
  /**
   * Development mode: the crash panel shows the error's name and message. Off by default, so a user
   * sees a generic message and no internals such as template or component names (security review L5);
   * the error itself always goes to `onError` or `reportError`.
   */
  dev?: boolean;
}

/**
 * The root props field: optional when the root's props type accepts `{}`, required otherwise (type
 * review M3). `exactOptionalPropertyTypes` stays off (type review L6): the vendored signals core does
 * not compile with it, and every optional field here treats `undefined` as absent anyway.
 */
type PropsField<P> = {} extends P ? { props?: P } : { props: P };

/** The options of `mount`: `templates`, the root props (typed from the root class), `onError`, `dev`. */
export type MountOptions<P = Record<string, unknown>> = MountBase & PropsField<P>;

/** The options the app reads, whatever the root's props type. */
type AppOptions = MountBase & { props?: object };

const apps = new WeakMap<Element, App>();

class App implements AppContext {
  readonly delegated = new Set<string>();
  private readonly templates = new Map<string, (self: object) => Node | null>();
  private readonly owner: Owner;
  private readonly boundary: Boundary;
  private readonly start: Text;
  private readonly end: Text;
  private crashed = false;
  private destroyed = false;
  private panel: Element | null = null;
  private frag: DocumentFragment | null = null;
  private rootScope: ComponentScope | null = null;
  private settle: { resolve: (c: Component<object>) => void; reject: (e: unknown) => void } | null = null;
  instance: Component<object> | null = null;

  constructor(
    readonly root: Element,
    private readonly options: AppOptions,
  ) {
    if (options === null || typeof options !== "object" || options.templates == null) {
      throw new UIError("mount: options.templates is required (the compiled template modules; [] for a built-in root)", "usage");
    }
    for (const t of options.templates) {
      if (typeof t?.name !== "string" || typeof t.render !== "function") {
        throw new UIError("mount: every template must be a compiled module ({name, render})", "template");
      }
      if (this.templates.has(t.name)) throw new UIError(`mount: template "${t.name}" is given twice`, "template");
      this.templates.set(t.name, t.render);
    }
    this.start = document.createTextNode("");
    this.end = document.createTextNode("");
    this.boundary = new Boundary(() => this.commit());
    this.owner = new Owner(null, this, this.boundary, this.boundary);
    if (options.env === undefined) this.owner.env = createEnv();
    else { validateEnv(options.env); this.owner.env = Object.freeze(options.env); }
  }

  template(name: string): ((self: object) => Node | null) | undefined {
    return this.templates.get(name);
  }

  run(Root: ComponentClass): Promise<Component<object>> {
    return new Promise<Component<object>>((resolve, reject) => {
      this.settle = { resolve, reject };
      const prev = setOwner(this.owner);
      let cs: ComponentScope;
      try {
        const frag = document.createDocumentFragment();
        frag.append(this.start, this.end);
        this.frag = frag;
        const thunks: Record<string, () => unknown> = {};
        const rawProps = (this.options.props ?? {}) as Record<string, unknown>;
        for (const [k, v] of Object.entries(rawProps)) {
          const desc = Object.getOwnPropertyDescriptor(rawProps, k);
          if (desc && typeof desc.get === "function") {
            const getter = desc.get;
            thunks[k] = () => getter.call(rawProps);
          } else {
            thunks[k] = () => v;
          }
        }
        cs = createComponent(Root, this.owner, frag, this.end, thunks, null, null);
      } catch (e) {
        // A failure outside the component's own error chain (a props getter that throws, an internal
        // error): nothing was mounted, so the target is free again and the promise rejects (review M2).
        this.settle = null;
        this.destroy();
        reject(e);
        return;
      } finally {
        setOwner(prev);
      }
      this.rootScope = cs;
      if (!this.crashed && this.boundary.pending === 0) this.boundary.tryCommit();
    });
  }

  private commit(): void {
    this.boundary.committed = true;
    if (this.crashed || this.destroyed) {
      // Unmounted (or crashed) while an onWillStart was pending: nothing goes into the root (review H1).
      this.frag = null;
      return;
    }
    this.root.appendChild(this.frag!);
    this.frag = null;
    mountTree(this.boundary);
    const instance = this.rootScope!.instance;
    this.instance = instance;
    this.settle?.resolve(instance);
    this.settle = null;
  }

  report(error: unknown): void {
    const onError = this.options.onError;
    if (onError === undefined) {
      reportUnhandled(error);
      return;
    }
    try {
      onError(error);
    } catch (e) {
      reportUnhandled(e);
    }
  }

  crash(error: unknown): void {
    if (this.destroyed || this.crashed) {
      // A destroyed app never paints over a root it gave back (review M1); a crashed one already shows
      // its panel. Either way the error is reported.
      this.report(error);
      return;
    }
    this.crashed = true;
    dispose(this.owner);
    this.frag = null;
    if (this.boundary.committed) {
      try {
        removeRange(this.start, this.end);
      } catch (e) {
        // Code outside the runtime broke the root's range: the panel still shows, and that is reported.
        this.report(e);
      }
    }
    const panel = document.createElement("div");
    panel.setAttribute("role", "alert");
    panel.className = "o_erp_crash";
    const p = panel.appendChild(document.createElement("p"));
    p.textContent = "The application stopped after an error. Reload the page to continue.";
    if (this.options.dev === true) {
      const detail = panel.appendChild(document.createElement("pre"));
      detail.textContent = error instanceof Error ? `${error.name}: ${error.message}` : String(error);
    }
    this.root.appendChild(panel);
    this.panel = panel;
    const settle = this.settle;
    if (settle !== null) {
      this.settle = null;
      settle.reject(error);
      if (this.options.onError !== undefined) this.report(error);
    } else {
      this.report(error);
    }
  }

  /** Disposes the app; a broken root range (code outside the runtime) is thrown after everything else is done. */
  destroy(): void {
    if (this.destroyed) return;
    this.destroyed = true;
    apps.delete(this.root);
    dispose(this.owner);
    this.frag = null;
    if (this.panel !== null) {
      this.panel.remove();
      this.panel = null;
    }
    if (this.settle !== null) {
      this.settle.reject(new UIError("unmount: the app was unmounted before it finished mounting", "usage"));
      this.settle = null;
    }
    if (this.boundary.committed && !this.crashed) removeRange(this.start, this.end);
  }
}

/**
 * Mounts `Root` at the end of `target` (§7.6). The promise resolves with the root instance once the
 * root boundary has committed (every `onWillStart` inside it resolved, `onMounted` run), or rejects with
 * the error that made the root show its crash panel. Every failure is a rejection, a second mount on the
 * same element included.
 */
export async function mount<C extends ComponentClass>(
  Root: C,
  target: Element,
  options: MountOptions<PropsOf<C>>,
): Promise<InstanceType<C>> {
  if (apps.has(target)) throw new UIError("mount: an app is already mounted on this element; unmount it first", "usage");
  const app = new App(target, options as AppOptions);
  apps.set(target, app);
  return (await app.run(Root)) as InstanceType<C>;
}

/** Destroys the app mounted on `target` (its components are disposed, its DOM or crash panel removed). */
export function unmount(target: Element): void {
  const app = apps.get(target);
  if (app === undefined) throw new UIError("unmount: no app is mounted on this element", "usage");
  app.destroy();
}

/** @internal Test harness admission: whether this target has an application to dispose. */
export function hasMountedApp(target: Element): boolean { return apps.has(target); }
