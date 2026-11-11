/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The frozen helper ABI of the Elang runtime. Compiled
// templates import these 17 helpers as `* as $ from "@erpforall/elang/abi"`; the list and the signatures
// are the compiler's side of the contract, and the two tasks'
// reviews check them against each other. A change here is an ABI change and goes through the lead.
// The implementation (abi-impl.ts) is checked against this file by tsc (abi-conformance.ts) and by a
// test that its exports are exactly these names (tests/abi.test.ts). Semantics:

/**
 * The loop row a compiled `t-foreach` body reads. The type parameters (item and value types) are a
 * type-only refinement for code that knows its collection (type review L3); the helpers use `Row`, that
 * is `Row<unknown, unknown>`.
 */
export interface Row<I = unknown, V = unknown> {
  item(): I;
  value(): V;
  index(): number;
  count(): number;
}

/** A slot a caller passes to a component (`render` read-only, type review L3). */
export interface Slot {
  readonly render: (scope?: object) => Node | null;
}

export declare function template(html: string, multi: 0 | 1): () => Node;
export declare function effect(fn: () => void): void;
export declare function insert(parent: Node, value: () => unknown, marker: Node | null): void;
export declare function out(
  parent: Node,
  value: () => unknown,
  marker: Node | null,
  fallback: () => Node | null,
): void;
export declare function attr(el: Element, name: string, value: unknown): void;
export declare function cls(el: Element, value: unknown): void;
export declare function style(el: Element, value: unknown): void;
export declare function prop(el: Element, name: string, value: unknown): void;
export declare function attrs(el: Element, value: unknown): void;
export declare function interp(...parts: unknown[]): string;
export declare function on(el: Element, event: string, handler: (ev: Event) => void, flags: number): void;
export declare function callHandler(fn: unknown, self: object, ev: Event): void;
export declare function cond(
  parent: Node,
  marker: Node | null,
  select: () => number,
  branches: Array<() => Node | null>,
): void;
export declare function list(
  parent: Node,
  marker: Node | null,
  items: () => unknown,
  key: (item: unknown, value: unknown, index: number, count: number) => unknown,
  row: (r: Row) => Node | null,
): void;
export declare function keyed(
  parent: Node,
  marker: Node | null,
  key: () => unknown,
  build: () => Node | null,
): void;
export declare function component(
  parent: Node,
  marker: Node | null,
  self: object,
  name: string,
  props: Record<string, () => unknown>,
  spread: (() => object) | null,
  slots: Record<string, Slot> | null,
): void;
export declare function slot(
  parent: Node,
  marker: Node | null,
  self: object,
  name: string,
  scope: (() => object) | null,
  fallback: (() => Node | null) | null,
): void;
