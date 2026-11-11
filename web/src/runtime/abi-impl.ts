/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The module compiled templates import (`@erpforall/elang/abi`, the compiler's `ABI_MODULE`): exactly the
// 17 helpers of abi.d.ts.

import { UIError } from "./errors.js";
import { requireOwner } from "./owner.js";
import { createEffect } from "./reactive.js";

export { template, insert, out, attr, cls, style, prop, attrs, interp } from "./dom.js";
export { on, callHandler } from "./events.js";
export { cond, list, keyed } from "./control.js";
export { component, slot } from "./component.js";

/** A binding: owned by the current owner; it disposes itself if it tracked nothing. */
export function effect(fn: () => void): void {
  if (typeof fn !== "function") throw new UIError(`effect takes a function, got ${typeof fn}`, "handler");
  createEffect(fn, requireOwner("effect"));
}
