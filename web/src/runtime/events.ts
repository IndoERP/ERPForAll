/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Event helpers of the ABI: `on` and `callHandler`.

import { UIError } from "./errors.js";
import { type Owner, enterCallback, handleError, leaveCallback, requireOwner, routeResult } from "./owner.js";

/** `on` flags (compiler note §7.2). */
export const STOP = 1;
export const PREVENT = 2;
export const SELF = 4;
export const CAPTURE = 8;
export const SYNTHETIC = 16;
export const PASSIVE = 32;

/** The event names `on` accepts (security review L3): lowercase letters, digits, `_`, `.`, `:`, `-`. */
const EVENT_NAME = /^[a-z0-9_.:-]+$/;

interface DelegatedEntry {
  readonly handler: (ev: Event) => unknown;
  readonly flags: number;
  readonly owner: Owner;
}

/**
 * Delegated (`synthetic`) handlers, per element and per listener key (event, capture, passive). A module
 * map, not properties on the elements: nothing is added to live DOM nodes.
 */
const delegatedHandlers = new WeakMap<Element, Map<string, DelegatedEntry[]>>();

/**
 * Applies the modifiers and calls the handler; returns whether `stop` ended propagation. `self` gates
 * everything: when the event did not start on `el`, nothing is prevented or stopped and the handler is
 * not called (the flags carry no order; OWL applies `.prevent.self` in written order, §12). A handler's
 * thrown error goes to its owner's error chain; a promise it returns that rejects goes there too, or to
 * the app's report channel when the owner was disposed meanwhile (review round 2, silent-failure 1).
 */
function dispatch(el: Element, ev: Event, handler: (ev: Event) => unknown, flags: number, owner: Owner): boolean {
  if (owner.disposed) return false;
  if (flags & SELF && ev.target !== el) return false;
  if (flags & PREVENT) ev.preventDefault();
  if (flags & STOP) ev.stopPropagation();
  const prev = enterCallback();
  try {
    routeResult(handler(ev), owner);
  } catch (e) {
    handleError(owner, e);
  } finally {
    leaveCallback(prev);
  }
  return (flags & STOP) !== 0;
}

/**
 * `t-on-<event>` (§7.2). Without `synthetic`, one listener on `el` (`capture` and `passive` passed to
 * `addEventListener`). With `synthetic`, the handler is recorded for `el` and one listener per event,
 * capture and passive combination on the render root walks from the target up to the root, calling each
 * recorded handler; `stop` ends the walk. Inside a delegated handler `ev.currentTarget` is the render
 * root, not `el` (the listener is on the root); a direct handler sees `el`. A passive and a non-passive
 * delegated handler of one event are two walks (two root listeners), each in bubbling order. A handler of
 * a disposed owner never runs. `prevent` together with `passive` is refused: the browser ignores
 * `preventDefault()` in a passive listener, so the pair would silently do nothing (review round 2,
 * silent-failure 8; the compiler refuses `.prevent.passive` too). The handler is typed as abi.d.ts
 * declares it; what it returns at run time is still routed (a promise's rejection reaches the owner).
 */
export function on(el: Element, event: string, handler: (ev: Event) => void, flags: number): void {
  if (typeof handler !== "function") {
    throw new UIError(`on: the handler of "${event}" must be a function, got ${typeof handler}`, "handler");
  }
  if (!EVENT_NAME.test(event)) {
    throw new UIError(
      `on: ${JSON.stringify(event)} is not an event name: use lowercase letters, digits, "_", ".", ":" and "-"`,
      "value",
    );
  }
  if ((flags & (PREVENT | PASSIVE)) === (PREVENT | PASSIVE)) {
    throw new UIError(
      `on: "${event}" has both prevent and passive: a passive listener cannot prevent the default action; drop one of them`,
      "value",
    );
  }
  const owner = requireOwner("on");
  if (flags & SYNTHETIC) {
    const key = `${event}${flags & CAPTURE ? "/capture" : ""}${flags & PASSIVE ? "/passive" : ""}`;
    // The root listener first: when it cannot be added, nothing is recorded (review round 2, finding 11).
    delegate(owner, event, key, flags);
    let byKey = delegatedHandlers.get(el);
    if (byKey === undefined) delegatedHandlers.set(el, (byKey = new Map()));
    let entries = byKey.get(key);
    if (entries === undefined) byKey.set(key, (entries = []));
    entries.push({ handler, flags, owner });
    return;
  }
  el.addEventListener(event, (ev) => dispatch(el, ev, handler, flags, owner), {
    capture: (flags & CAPTURE) !== 0,
    passive: (flags & PASSIVE) !== 0,
  });
}

/**
 * Adds the root listener of `key` once per app. The key is marked only after `addEventListener`
 * succeeded, so a failed attempt (no render root) fails again next time instead of recording handlers
 * that can never run (review round 2, silent-failure 11).
 */
function delegate(owner: Owner, event: string, key: string, flags: number): void {
  const app = owner.app;
  if (app.delegated.has(key)) return;
  const root = app.root;
  root.addEventListener(
    event,
    (ev) => {
      let node = ev.target as Node | null;
      while (node !== null) {
        const entries = delegatedHandlers.get(node as Element)?.get(key);
        if (entries !== undefined) {
          let stopped = false;
          for (let i = 0; i < entries.length; i++) {
            const e = entries[i]!;
            if (dispatch(node as Element, ev, e.handler, e.flags, e.owner)) stopped = true;
          }
          if (stopped) return;
        }
        if (node === root) return;
        node = node.parentNode;
      }
    },
    { capture: (flags & CAPTURE) !== 0, passive: (flags & PASSIVE) !== 0 },
  );
  app.delegated.add(key);
}

/**
 * Calls a `t-on` expression that is not an arrow function with the component as `this`. Typed `void` as
 * abi.d.ts declares it, but what the function returns is passed through at run time, so a promise
 * reaches `on`, which routes its rejection. A value that is not a function is's error, OWL's
 * text (owl.js:4173-4180).
 */
export function callHandler(fn: unknown, self: object, ev: Event): void {
  if (typeof fn !== "function") {
    throw new UIError(
      `Invalid handler expression: the \`t-on\` expression should evaluate to a function, but got '${typeof fn}'. Did you mean to use an arrow function? (e.g. \`t-on-click="() => expr"\`)`,
      "handler",
    );
  }
  return (fn as (this: object, ev: Event) => unknown).call(self, ev) as void;
}
