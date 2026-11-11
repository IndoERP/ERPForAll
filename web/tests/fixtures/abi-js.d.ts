/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The frozen ABI as compiled JavaScript sees it, for the fixture check (tsconfig.fixtures.json, type review
// M4). Derived from abi.d.ts by a type transform, never written by hand: every helper keeps its parameter
// list, arity and the kind of each argument (a function, a string, a number, a node, a record of thunks),
// so a drift in the compiler's calls fails `tsc`. Only what JavaScript cannot express is relaxed: a data
// position typed `unknown` or `object` (a row's item, a slot's scope, `$self`) is `any`, because compiled
// code reads properties of it, and an `Element` position takes any `Node`, because the compiler reaches
// an element through `firstChild`/`nextSibling`, which the DOM types as a `ChildNode`.

import type * as Contract from "../../src/runtime/abi.js";

type Js<T> = unknown extends T
  ? any
  : object extends T
    ? any
    : T extends Element
      ? Node
      : T extends (...args: infer A) => infer R
        ? (...args: { [K in keyof A]: Js<A[K]> }) => Js<R>
        : T extends readonly (infer E)[]
          ? Js<E>[]
          : T extends Contract.Row | Contract.Slot
            ? { [K in keyof T]: Js<T[K]> }
            : T extends Record<string, infer V>
              ? Record<string, Js<V>>
              : T;

declare const abi: { readonly [K in keyof typeof Contract]: Js<(typeof Contract)[K]> };
export = abi;
