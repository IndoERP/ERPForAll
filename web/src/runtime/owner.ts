/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Owners: the unit of disposal and error routing. Every effect,
// listener and structure belongs to the owner that was current when it was created. Owners keep no
// child lists: a structure that creates child owners registers one cleanup on its own owner that
// disposes them, so removing a list row costs O(1) apart from its DOM.

import { trackTestWork } from "./test-mutations.js";
import type { Env } from "./env.js";
import { UIError } from "./errors.js";

/** What an owner tree needs from the app it belongs to. */
export interface AppContext {
  /** The render root: the mount target, where delegated (`synthetic`) listeners live. */
  readonly root: Element;
  /** Delegated event keys already listened to on `root`. */
  readonly delegated: Set<string>;
  /** An error no `onError` handler took: the root shows its crash panel and reports it. */
  crash(error: unknown): void;
  /**
   * An error to report without crashing: one a boundary could not show because it was already
   * switching to its fallback for an earlier error of the same build (mount option `onError`, else
   * `globalThis.reportError`).
   */
  report(error: unknown): void;
  /** The compiled template registered under `name`, if any. */
  template(name: string): ((self: object) => Node | null) | undefined;
}

/**
 * A scope whose components wait to run `onMounted` together (§7.6). A component is one; a pending
 * boundary's content is another. `kids` lists the scopes created inside it, in creation order, until
 * it is mounted; `mounted` turns true when its DOM is in the document and its hooks have run.
 */
export class MountScope {
  kids: MountScope[] | null = [];
  mounted = false;
  /** False while a component waits for `onWillStart`: `mountTree` skips it and it mounts itself later. */
  rendered = true;
  /** False once its component was disposed: it never mounts. */
  alive = true;

  /** The scope's own `onMounted` hooks (components override). */
  runMounted(): void {}
}

/**
 * A pending boundary (the root, or a `Pending` component's content): it holds its content out of the
 * document while `pending` components inside it wait for `onWillStart`, and commits when the count
 * returns to zero.
 */
export class Boundary extends MountScope {
  pending = 0;
  committed = false;

  constructor(private readonly onReady: () => void) {
    super();
    this.rendered = false;
  }

  /** One fewer component waiting; commits when none is left. */
  release(): void {
    this.pending--;
    this.tryCommit();
  }

  /** Commits now when no component is waiting (after the boundary's content was built). */
  tryCommit(): void {
    if (this.pending === 0 && !this.committed) this.onReady();
  }
}

export class Owner {
  /** Inherited environment, overridden only by setup-scoped useSubEnv. */
  env!: Env;
  readonly depth: number;
  /** The component of this owner, when it is a component's own owner. */
  comp: ComponentState | null = null;
  effects: Array<() => void> | null = null;
  cleanups: Array<() => void> | null = null;
  disposed = false;

  constructor(
    readonly parent: Owner | null,
    readonly app: AppContext,
    /** The nearest pending boundary (async `onWillStart` counts against it while it is not committed). */
    public boundary: Boundary | null,
    /** Where components created under this owner register for `onMounted`. */
    public scope: MountScope,
  ) {
    this.depth = parent === null ? 0 : parent.depth + 1;
    if (parent !== null) this.env = parent.env;
  }

  /** A child owner (same app, boundary and mount scope). */
  child(): Owner {
    return new Owner(this, this.app, this.boundary, this.scope);
  }

  onCleanup(fn: () => void): void {
    (this.cleanups ??= []).push(fn);
  }
}

/** A lifecycle hook: called with the component instance as `this`; what it returns is routed (§5). */
export type Hook = (this: object) => unknown;

/** The per-component part of an owner (filled by component.ts). */
export interface ComponentState {
  readonly instance: object;
  readonly scope: MountScope;
  errorHandlers: Array<(error: unknown) => void> | null;
  willUnmount: Hook[] | null;
  willDestroy: Hook[] | null;
}

/**
 * Mounts `scope` (§7.6): first every rendered, live scope it collected, in creation order (so children
 * run `onMounted` before their parents), then its own hooks. A scope still waiting for
 * `onWillStart` is skipped and mounts itself when it renders.
 */
export function mountTree(scope: MountScope): void {
  if (!scope.alive || scope.mounted) return;
  const kids = scope.kids;
  scope.kids = null;
  if (kids !== null) {
    for (let i = 0; i < kids.length; i++) {
      const kid = kids[i]!;
      if (kid.rendered) mountTree(kid);
    }
  }
  scope.mounted = true;
  scope.runMounted();
}

let current: Owner | null = null;

/** The owner new effects, listeners and structures belong to. */
export function getOwner(): Owner | null {
  return current;
}

/** Makes `owner` current and returns the previous one (restore it in a `finally`). */
export function setOwner(owner: Owner | null): Owner | null {
  const prev = current;
  current = owner;
  return prev;
}

/** The current owner, or an error naming the helper that needs one. */
export function requireOwner(what: string): Owner {
  if (current === null) {
    throw new UIError(`${what} needs a current owner: call it while a component renders or inside mount()`, "usage");
  }
  return current;
}

/**
 * Component callbacks running now (event handlers, lifecycle hooks, `onError` handlers). A callback runs
 * with no current owner, whatever structure triggered it, so `effect()` inside one is always refused
 * instead of creating an effect no component owns (review round 2, silent-failure 10).
 */
let callbacks = 0;

/**
 * Marks the start of a component callback and clears the current owner; returns the previous owner,
 * which `leaveCallback` restores (call it in a `finally`).
 */
export function enterCallback(): Owner | null {
  callbacks++;
  const prev = current;
  current = null;
  return prev;
}

export function leaveCallback(prev: Owner | null): void {
  callbacks--;
  current = prev;
}

/** Whether a component callback (handler, hook, `onError` handler) is running. */
export function inCallback(): boolean {
  return callbacks > 0;
}

/** Whether `value` is a thenable (a promise or a promise-like). */
export function isThenable(value: unknown): value is PromiseLike<unknown> {
  return (
    value !== null &&
    (typeof value === "object" || typeof value === "function") &&
    typeof (value as PromiseLike<unknown>).then === "function"
  );
}

/**
 * Routes an error that arrives later (a rejected promise, an error after an await): to the owner's
 * error chain while the owner lives, else to the app's report channel (mount option `onError`, else
 * `globalThis.reportError`). Never dropped, also once the owner is gone (review round 2, silent-failure 1).
 */
export function routeError(owner: Owner, error: unknown): void {
  if (owner.disposed) owner.app.report(error);
  else handleError(owner, error);
}

/**
 * Routes what a handler, hook or effect returned: a thenable's rejection goes to `routeError(owner)`
 * (review round 2, silent-failure 3); any other value is ignored.
 */
export function routeResult(result: unknown, owner: Owner): void {
  if (isThenable(result)) {
    trackTestWork(result, owner.disposed).then(undefined, (e: unknown) => {
      routeError(owner, e);
    });
  }
}

/**
 * Routes `error` from `owner` up the owner chain (OWL `invokeErrorHandlers`, owl.js:1779-1797): each
 * component's `onError` handlers in reverse registration order; a handler that returns has handled it,
 * a handler that throws replaces the error and the walk continues. The handlers of an owner that is
 * disposed (or being disposed) are skipped: a component on its way out never takes an error, so a
 * teardown error goes on to the living ancestors (review round 2, silent-failure 2). An error nobody
 * handles reaches the app's crash path, which reports it when the app is destroyed or has crashed; it
 * is never dropped.
 */
export function handleError(owner: Owner, error: unknown): void {
  let o: Owner | null = owner;
  while (o !== null) {
    const handlers = o.disposed ? null : o.comp?.errorHandlers;
    if (handlers) {
      for (let i = handlers.length - 1; i >= 0; i--) {
        const prev = enterCallback();
        try {
          handlers[i]!(error);
          return;
        } catch (e) {
          error = e;
        } finally {
          leaveCallback(prev);
        }
      }
    }
    o = o.parent;
  }
  owner.app.crash(error);
}

/**
 * Disposes `owner` (OWL `_destroy`/`finalize` order, owl.js:3809-3824, 387-408): its component's
 * `onWillUnmount` hooks in reverse registration order while its DOM is still attached (parents before
 * children), its effects, its cleanups in reverse order (child owners, which repeat this), then its
 * component's `onWillDestroy` hooks in reverse order (children before parents). A hook or cleanup that
 * throws is reported through `handleError` (which skips the disposed owners, so a living ancestor or the
 * app gets it) and disposal continues; a hook's rejected promise is reported the same way. Disposal never
 * touches the DOM: the structure removing a block detaches its range afterwards.
 */
export function dispose(owner: Owner): void {
  if (owner.disposed) return;
  owner.disposed = true;
  const comp = owner.comp;
  if (comp !== null && comp.scope.mounted && comp.willUnmount !== null) {
    runHooks(owner, comp.willUnmount, comp.instance);
  }
  const effects = owner.effects;
  if (effects !== null) {
    owner.effects = null;
    for (let i = 0; i < effects.length; i++) effects[i]!();
  }
  const cleanups = owner.cleanups;
  if (cleanups !== null) {
    owner.cleanups = null;
    for (let i = cleanups.length - 1; i >= 0; i--) {
      try {
        cleanups[i]!();
      } catch (e) {
        handleError(owner, e);
      }
    }
  }
  if (comp !== null) {
    comp.scope.mounted = false;
    comp.scope.alive = false;
    comp.scope.kids = null;
    if (comp.willDestroy !== null) runHooks(owner, comp.willDestroy, comp.instance);
  }
}

function runHooks(owner: Owner, hooks: Hook[], self: object): void {
  for (let i = hooks.length - 1; i >= 0; i--) {
    const prev = enterCallback();
    try {
      routeResult(hooks[i]!.call(self), owner);
    } catch (e) {
      handleError(owner, e);
    } finally {
      leaveCallback(prev);
    }
  }
}
