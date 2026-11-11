/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Structural helpers of the ABI (§7.3): cond, keyed, list.

import reconcileArrays from "../../vendor/dom-expressions/src/reconcile.js";
import {
  Region,
  beginBuild,
  blockRange,
  containerOf,
  endAnchor,
  endBuild,
  insertBefore,
} from "./block.js";
import { signal as coreSignal } from "../../vendor/alien-signals/src/index.js";
import type { Row } from "./abi.js";
import { UIError } from "./errors.js";
import { type MountScope, type Owner, dispose, handleError, requireOwner, setOwner } from "./owner.js";
import { type CoreSignal, counters, createEffect, untrack } from "./reactive.js";

export type { Row };

// ---------------------------------------------------------------------------------------------------
// cond, keyed

/**
 * `t-if`/`t-elif`/`t-else` (§7.2): an effect evaluates `select()`; only when the index changes is the
 * old branch disposed and removed and the new one built in a child owner and inserted. `-1` renders
 * nothing.
 */
export function cond(
  parent: Node,
  marker: Node | null,
  select: () => number,
  branches: Array<() => Node | null>,
): void {
  const owner = requireOwner("cond");
  const end = endAnchor(parent, marker);
  const region = new Region(parent, end);
  let index = -1;
  owner.onCleanup(() => region.dispose());
  createEffect(() => {
    const i = select();
    if (i === index) return;
    if (i !== -1 && (i < 0 || i >= branches.length || !Number.isInteger(i))) {
      throw new UIError(`cond: branch index ${String(i)} is out of range (0..${branches.length - 1} or -1)`, "value");
    }
    region.clear();
    index = i;
    if (i !== -1) region.build(branches[i]!, owner);
  }, owner);
}

/** `t-key` on a block (§7.2): rebuilds the block when `key()` changes (`Object.is`). */
export function keyed(parent: Node, marker: Node | null, key: () => unknown, build: () => Node | null): void {
  const owner = requireOwner("keyed");
  const end = endAnchor(parent, marker);
  const region = new Region(parent, end);
  let built = false;
  let current: unknown;
  owner.onCleanup(() => region.dispose());
  createEffect(() => {
    const k = key();
    if (built && Object.is(k, current)) return;
    region.clear();
    built = true;
    current = k;
    region.build(build, owner);
  }, owner);
}

// ---------------------------------------------------------------------------------------------------
// list

class ListRow implements Row {
  owner: Owner | null = null;
  first: Node | null = null;
  last: Node | null = null;
  private readonly $item: CoreSignal<unknown>;
  private readonly $value: CoreSignal<unknown> | null;
  private readonly $index: CoreSignal<number>;

  constructor(
    readonly key: unknown,
    item: unknown,
    value: unknown,
    index: number,
    hasValue: boolean,
    private readonly $count: CoreSignal<number>,
  ) {
    this.$item = coreSignal<unknown>(item);
    this.$value = hasValue ? coreSignal<unknown>(value) : null;
    this.$index = coreSignal(index);
    counters.signals += hasValue ? 3 : 2;
  }

  item(): unknown {
    return this.$item();
  }
  value(): unknown {
    return this.$value === null ? this.$item() : this.$value();
  }
  index(): number {
    return this.$index();
  }
  count(): number {
    return this.$count();
  }

  update(item: unknown, value: unknown, index: number): void {
    this.$item(item);
    if (this.$value !== null) this.$value(value);
    this.$index(index);
  }

  /** The row's current nodes, in order (a range can grow inside between its ends). */
  nodes(out: Node[]): void {
    const first = this.first;
    if (first === null) return;
    if (first === this.last) {
      out.push(first);
      return;
    }
    let n: Node | null = first;
    while (n !== null) {
      out.push(n);
      if (n === this.last) return;
      n = n.nextSibling;
    }
  }
}

/** Whether `next` starts with every row of `old`, in the same order (an append). */
function keptPrefix(old: ListRow[], next: ListRow[]): boolean {
  if (next.length < old.length) return false;
  for (let i = 0; i < old.length; i++) if (old[i] !== next[i]) return false;
  return true;
}

/** A `t-foreach` collection read as OWL's `prepareList` reads it (owl.js:4070-4090). */
function prepare(collection: unknown): { items: unknown[]; values: unknown[]; hasValue: boolean } {
  if (Array.isArray(collection)) return { items: collection, values: collection, hasValue: false };
  if (collection instanceof Map) {
    return { items: [...collection.keys()], values: [...collection.values()], hasValue: true };
  }
  if (collection !== null && collection !== undefined && Symbol.iterator in Object(collection)) {
    const items = [...(collection as Iterable<unknown>)];
    return { items, values: items, hasValue: false };
  }
  if (collection !== null && typeof collection === "object") {
    return { items: Object.keys(collection), values: Object.values(collection), hasValue: true };
  }
  throw new UIError(`Invalid loop expression: "${String(collection)}" is not iterable`, "loop");
}

/**
 * `t-foreach` with `t-key` (§7.3). Rows are kept by key: a kept row gets its `item`/`value`/`index`
 * signals written (only the bindings reading a changed one update), removed rows are disposed, new rows
 * are built in their own owner, and the DOM is moved with the vendored `reconcileArrays` (udomdiff,
 * fewest moves). A duplicate key is's error in every mode, a non-iterable collection
 */
export function list(
  parent: Node,
  marker: Node | null,
  items: () => unknown,
  key: (item: unknown, value: unknown, index: number, count: number) => unknown,
  row: (r: Row) => Node | null,
): void {
  const owner = requireOwner("list");
  const end = endAnchor(parent, marker);
  const $count = coreSignal(0);
  let rows: ListRow[] = [];
  let byKey = new Map<unknown, ListRow>();
  owner.onCleanup(() => {
    for (const r of rows) if (r.owner !== null) dispose(r.owner);
  });

  /** Builds a new row in its own owner; its nodes stay where the build put them until placeRows. */
  const build = (r: ListRow, scope: MountScope | null): void => {
    const o = owner.child();
    if (scope !== null) o.scope = scope;
    r.owner = o;
    const prev = setOwner(o);
    let node: Node | null = null;
    try {
      node = row(r);
    } catch (e) {
      handleError(o, e);
    } finally {
      setOwner(prev);
    }
    const range = o.disposed ? null : blockRange(node);
    if (range !== null) {
      r.first = range[0];
      r.last = range[1];
    }
  };

  const placeRows = (old: ListRow[], next: ListRow[], kept: number): void => {
    const container = containerOf(parent, end);
    const a: Node[] = [];
    for (const r of old) r.nodes(a);
    if (kept === 0 && a.length > 0) {
      // Nothing kept (clear, replace): drop the old nodes in one step where the list owns the element.
      if (end === null && container.firstChild === a[0]) container.textContent = "";
      else for (const node of a) container.removeChild(node);
      a.length = 0;
    }
    if (a.length === 0 || (kept === old.length && keptPrefix(old, next))) {
      // Nothing left to move: the new rows go in as one fragment (create, replace, append after the kept rows).
      const frag = document.createDocumentFragment();
      for (let i = a.length === 0 ? 0 : old.length; i < next.length; i++) {
        const nodes: Node[] = [];
        next[i]!.nodes(nodes);
        for (const node of nodes) frag.appendChild(node);
      }
      if (frag.firstChild !== null) insertBefore(parent, end, frag);
      return;
    }
    const b: Node[] = [];
    for (const r of next) r.nodes(b);
    if (b.length === 0) {
      for (const node of a) container.removeChild(node);
      return;
    }
    reconcileArrays(container, a as ChildNode[], b as ChildNode[]);
  };

  const update = (collection: unknown): void => {
    const { items: its, values, hasValue } = prepare(collection);
    const n = its.length;
    const keys = new Array<unknown>(n);
    const seen = new Set<unknown>();
    for (let i = 0; i < n; i++) {
      const k = key(its[i], values[i], i, n);
      if (seen.has(k)) throw new UIError(`Got duplicate key in t-foreach: ${String(k)}`, "loop");
      seen.add(k);
      keys[i] = k;
    }
    const old = rows;
    const next = new Array<ListRow>(n);
    const nextByKey = new Map<unknown, ListRow>();
    const scope = beginBuild(owner);
    let sameOrder = old.length === n;
    let kept = 0;
    $count(n);
    for (let i = 0; i < n; i++) {
      const k = keys[i];
      let r = byKey.get(k);
      if (r !== undefined) {
        r.update(its[i], values[i], i);
        kept++;
        if (old[i] !== r) sameOrder = false;
      } else {
        sameOrder = false;
        r = new ListRow(k, its[i], values[i], i, hasValue, $count);
        build(r, scope);
        if (owner.disposed) {
          // A row's error made a boundary dispose this list mid-update (review M3): its cleanup disposed
          // the old rows only, so dispose the rows built in this pass, which no owner holds yet.
          for (let j = 0; j < i; j++) {
            const built = next[j]!;
            if (!byKey.has(built.key) && built.owner !== null) dispose(built.owner);
          }
          if (r.owner !== null) dispose(r.owner);
          return;
        }
      }
      next[i] = r;
      nextByKey.set(k, r);
    }
    for (const r of old) {
      if (!nextByKey.has(r.key) && r.owner !== null) dispose(r.owner);
    }
    if (!sameOrder) placeRows(old, next, kept);
    rows = next;
    byKey = nextByKey;
    endBuild(scope);
  };

  createEffect(() => {
    const collection = items();
    untrack(() => update(collection));
  }, owner);
}
