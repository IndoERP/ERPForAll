/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Reactive store proxies, columnar record signals, and keyed selectors.

import { getActiveSub, watchedSignal, signal as coreSignal } from "../../vendor/alien-signals/src/index.js";
import { UIError } from "./errors.js";
import { getOwner } from "./owner.js";
import { type CoreSignal, batch, effect, untrack } from "./reactive.js";

const RAW_SYMBOL: unique symbol = Symbol("erp.raw");

/** Marks an object as never-wrap (immutable / exempt from store proxying). */
export function raw<T extends object>(obj: T): T {
  Object.defineProperty(obj, RAW_SYMBOL, {
    value: true,
    enumerable: false,
    writable: false,
    configurable: true,
  });
  return obj;
}

/** Every store proxy and its raw target (objects, arrays, Maps and Sets alike, review round 3, R13). */
const proxyToRaw = new WeakMap<object, object>();
/** Every raw target and its store proxy, so one target has one proxy. */
const proxyCache = new WeakMap<object, object>();

/** Retrieves the underlying raw object from a store proxy; any other value is returned unchanged. */
export function toRaw<T>(obj: T): T {
  if (obj !== null && typeof obj === "object") {
    const target = proxyToRaw.get(obj);
    if (target !== undefined) return target as T;
  }
  return obj;
}

function isExempt(obj: unknown): boolean {
  if (obj === null || typeof obj !== "object") return true;
  if ((obj as { [RAW_SYMBOL]?: boolean })[RAW_SYMBOL] === true) return true;
  if (Object.isFrozen(obj)) return true;
  if (obj instanceof Date || obj instanceof RegExp || ArrayBuffer.isView(obj)) return true;

  // Custom class instances (prototype != Object.prototype and != Array.prototype and not Map/Set)
  const proto = Object.getPrototypeOf(obj);
  if (
    proto !== Object.prototype &&
    proto !== Array.prototype &&
    !(obj instanceof Map) &&
    !(obj instanceof Set)
  ) {
    return true;
  }
  return false;
}

/** A value as a store hands it out: a wrappable object as its store, anything else unchanged. */
function wrap<V>(value: V): V {
  return value !== null && typeof value === "object" ? store(value) : value;
}

const hasOwn = (o: object, key: PropertyKey): boolean => Object.prototype.hasOwnProperty.call(o, key);

/** The signal of `key` in `signals`, created with `initial()` on first use. */
function signalOf<K, V>(signals: Map<K, CoreSignal<V>>, key: K, initial: () => V): CoreSignal<V> {
  let s = signals.get(key);
  if (s === undefined) {
    s = coreSignal(initial());
    signals.set(key, s);
  }
  return s;
}

/** Whether `key` is an array index (a canonical non-negative integer string). */
function isIndex(key: PropertyKey): key is string {
  return typeof key === "string" && key !== "" && String(Number(key) >>> 0) === key;
}

/**
 * Creates a lazily wrapped reactive proxy for mutable objects, arrays, Maps, or Sets.
 * Implements the §4.4 depth rules.
 */
export function store<T extends object>(target: T): T {
  if (proxyToRaw.has(target)) return target;
  if (isExempt(target)) return target;

  const existing = proxyCache.get(target);
  if (existing !== undefined) return existing as T;

  let proxy: object;
  if (target instanceof Map) proxy = createMapStore(target);
  else if (target instanceof Set) proxy = createSetStore(target);
  else proxy = createObjectStore(target);
  proxyCache.set(target, proxy);
  proxyToRaw.set(proxy, target);
  return proxy as T;
}

/**
 * A plain object or array store: one value signal per key read, one presence signal per
 * key tested with `in`, a key-list signal for `Object.keys` and friends, and for arrays a `length`
 * signal. Adding or deleting a key notifies `in` and key-list readers, also when the key holds
 * `undefined`; shrinking an array notifies the readers of every index it removed (review round 3, R4,
 * R22). Signals are created on first read only.
 */
function createObjectStore<T extends object>(target: T): T {
  const values = new Map<PropertyKey, CoreSignal<unknown>>();
  const present = new Map<PropertyKey, CoreSignal<boolean>>();
  const isArray = Array.isArray(target);
  const keys = coreSignal(0);
  let keyVersion = 0;
  let lengthSignal: CoreSignal<number> | undefined;
  const arrayLength = (): number => (target as unknown as unknown[]).length;

  /** Notifies the readers of indices at or past `length` (they no longer exist). */
  const truncated = (length: number): void => {
    for (const [k, s] of values) if (isIndex(k) && Number(k) >= length) s(undefined);
    for (const [k, s] of present) if (isIndex(k) && Number(k) >= length) s(false);
  };

  return new Proxy(target, {
    get(t, prop, receiver) {
      if (isArray && prop === "length") {
        (lengthSignal ??= coreSignal(arrayLength()))();
        return arrayLength();
      }
      return wrap(signalOf(values, prop, () => Reflect.get(t, prop, receiver))());
    },
    has(t, prop) {
      return signalOf(present, prop, () => Reflect.has(t, prop))();
    },
    set(t, prop, value) {
      const had = hasOwn(t, prop);
      if (had && Reflect.get(t, prop) === value) return true;
      const oldLength = isArray ? arrayLength() : 0;
      if (!Reflect.set(t, prop, value)) return false;
      batch(() => {
        if (!(isArray && prop === "length")) values.get(prop)?.(value);
        if (!had) {
          present.get(prop)?.(true);
          keys(++keyVersion);
        }
        if (isArray) {
          const length = arrayLength();
          if (length !== oldLength) {
            lengthSignal?.(length);
            if (length < oldLength) {
              truncated(length);
              keys(++keyVersion);
            }
          }
        }
      });
      return true;
    },
    deleteProperty(t, prop) {
      if (!hasOwn(t, prop)) return true;
      if (!Reflect.deleteProperty(t, prop)) return false;
      batch(() => {
        values.get(prop)?.(undefined);
        present.get(prop)?.(false);
        keys(++keyVersion);
      });
      return true;
    },
    ownKeys(t) {
      keys();
      if (isArray) (lengthSignal ??= coreSignal(arrayLength()))();
      return Reflect.ownKeys(t);
    },
  });
}

/** An iterator over `source` that hands out each item through `f`. */
function* mapped<A, B>(source: Iterable<A>, f: (a: A) => B): Generator<B, undefined, unknown> {
  for (const a of source) yield f(a);
  return undefined;
}

/**
 * A Map store: per-key value and presence signals, a `size` signal, and an iteration signal that every
 * change notifies (an add, a delete, a value replaced, a clear), so `keys()`, `values()`, `entries()`,
 * `forEach` and `for…of` readers re-run on any change; values come out as stores, keys raw (review
 * round 3, R12, R22).
 */
function createMapStore<K, V>(map: Map<K, V>): Map<K, V> {
  const values = new Map<K, CoreSignal<unknown>>();
  const present = new Map<K, CoreSignal<boolean>>();
  const size = coreSignal(map.size);
  const iteration = coreSignal(0);
  let version = 0;
  let proxy!: Map<K, V>;

  const methods: Record<PropertyKey, unknown> = {
    get(key: K): V | undefined {
      return wrap(signalOf(values, key, () => map.get(key))() as V | undefined);
    },
    has(key: K): boolean {
      return signalOf(present, key, () => map.has(key))();
    },
    set(key: K, value: V): Map<K, V> {
      const had = map.has(key);
      if (had && map.get(key) === value) return proxy;
      map.set(key, value);
      batch(() => {
        values.get(key)?.(value);
        if (!had) {
          present.get(key)?.(true);
          size(map.size);
        }
        iteration(++version);
      });
      return proxy;
    },
    delete(key: K): boolean {
      if (!map.delete(key)) return false;
      batch(() => {
        values.get(key)?.(undefined);
        present.get(key)?.(false);
        size(map.size);
        iteration(++version);
      });
      return true;
    },
    clear(): void {
      if (map.size === 0) return;
      map.clear();
      batch(() => {
        for (const s of values.values()) s(undefined);
        for (const s of present.values()) s(false);
        size(0);
        iteration(++version);
      });
    },
    forEach(callback: (value: V, key: K, m: Map<K, V>) => void, thisArg?: unknown): void {
      iteration();
      map.forEach((value, key) => callback.call(thisArg, wrap(value), key, proxy));
    },
    keys(): Iterator<K> {
      iteration();
      return map.keys();
    },
    values(): Iterator<V> {
      iteration();
      return mapped(map.values(), wrap);
    },
    entries(): Iterator<[K, V]> {
      iteration();
      return mapped(map.entries(), ([k, v]): [K, V] => [k, wrap(v)]);
    },
    [Symbol.iterator](): Iterator<[K, V]> {
      iteration();
      return mapped(map.entries(), ([k, v]): [K, V] => [k, wrap(v)]);
    },
  };

  proxy = new Proxy(map, {
    get(target, prop) {
      if (prop === "size") {
        size();
        return target.size;
      }
      if (hasOwn(methods, prop)) return methods[prop];
      const val = Reflect.get(target, prop, target);
      return typeof val === "function" ? val.bind(target) : val;
    },
  });
  return proxy;
}

/**
 * A Set store: per-value presence signals, a `size` signal, and an iteration signal that every add,
 * delete and clear notifies, so iteration readers re-run also when a delete and an add keep the size
 * (review round 3, R12). Members are identities, so iteration yields them raw.
 */
function createSetStore<T>(set: Set<T>): Set<T> {
  const present = new Map<T, CoreSignal<boolean>>();
  const size = coreSignal(set.size);
  const iteration = coreSignal(0);
  let version = 0;
  let proxy!: Set<T>;

  const iterate = <R>(f: () => R): R => {
    iteration();
    return f();
  };
  const methods: Record<PropertyKey, unknown> = {
    has(value: T): boolean {
      return signalOf(present, value, () => set.has(value))();
    },
    add(value: T): Set<T> {
      if (set.has(value)) return proxy;
      set.add(value);
      batch(() => {
        present.get(value)?.(true);
        size(set.size);
        iteration(++version);
      });
      return proxy;
    },
    delete(value: T): boolean {
      if (!set.delete(value)) return false;
      batch(() => {
        present.get(value)?.(false);
        size(set.size);
        iteration(++version);
      });
      return true;
    },
    clear(): void {
      if (set.size === 0) return;
      set.clear();
      batch(() => {
        for (const s of present.values()) s(false);
        size(0);
        iteration(++version);
      });
    },
    forEach(callback: (value: T, key: T, s: Set<T>) => void, thisArg?: unknown): void {
      iterate(() => set.forEach((value) => callback.call(thisArg, value, value, proxy)));
    },
    keys: () => iterate(() => set.keys()),
    values: () => iterate(() => set.values()),
    entries: () => iterate(() => set.entries()),
    [Symbol.iterator]: () => iterate(() => set.values()),
  };

  proxy = new Proxy(set, {
    get(target, prop) {
      if (prop === "size") {
        size();
        return target.size;
      }
      if (hasOwn(methods, prop)) return methods[prop];
      const val = Reflect.get(target, prop, target);
      return typeof val === "function" ? val.bind(target) : val;
    },
  });
  return proxy;
}

/**
 * Dense two-dimensional reactive table for relational records (recordId, fieldName).
 * Signals are allocated only when a binding reads a coordinate (§3.2).
 */
export class ColumnarRecordStore {
  private values = new Map<string, unknown>();
  private signals = new Map<string, CoreSignal<unknown>>();

  private coord(recordId: string | number, field: string): string {
    return `${recordId}\x00${field}`;
  }

  /** Retained reactive coordinates, including cached computed dependencies. */
  get signalCount(): number {
    return this.signals.size;
  }

  get(recordId: string | number, field: string): unknown {
    const key = this.coord(recordId, field);
    if (getActiveSub() === undefined) return this.values.get(key);
    let s = this.signals.get(key);
    if (s === undefined) {
      const created = watchedSignal(this.values.get(key), () => {
        if (this.signals.get(key) === created) this.signals.delete(key);
      });
      s = created;
      this.signals.set(key, s);
    }
    return s();
  }

  set(recordId: string | number, field: string, value: unknown): void {
    const key = this.coord(recordId, field);
    this.values.set(key, value);
    const s = this.signals.get(key);
    if (s !== undefined) {
      s(value);
    }
  }

  has(recordId: string | number, field: string): boolean {
    return this.values.has(this.coord(recordId, field));
  }

  /**
   * Removes every value of `recordId`. The signals of read coordinates are kept and set to undefined:
   * a reader may still be subscribed, and a later `set()` must reach it (review round 3, R3). Signals
   * remain while subscribed, then the core's final-unwatch callback removes them. Untracked direct
   * reads allocate no signal. A cached computed remains a legitimate subscriber until it unlinks.
   */
  deleteRecord(recordId: string | number): void {
    const prefix = `${recordId}\x00`;
    this.deleteWhere((key) => key.startsWith(prefix));
  }

  /** Removes every value of `field`; read coordinates keep their signals (see `deleteRecord`). */
  deleteField(field: string): void {
    const suffix = `\x00${field}`;
    this.deleteWhere((key) => key.endsWith(suffix));
  }

  private deleteWhere(matches: (key: string) => boolean): void {
    batch(() => {
      for (const key of [...this.values.keys()]) {
        if (!matches(key)) continue;
        this.values.delete(key);
        this.signals.get(key)?.(undefined);
      }
    });
  }
}

/** A record id in a keyed table: a safe integer (server records) or a non-empty string (local records). */
export type RecordId = number | string;

const TABLE_TOKEN: unique symbol = Symbol("ModelTable");

interface KeyedRow {
  readonly values: Map<string, unknown>;
  signals: Map<string, CoreSignal<unknown>> | undefined;
}

function describeKey(value: unknown): string {
  switch (typeof value) {
    case "string":
      return JSON.stringify(value);
    case "bigint":
      return `${value}n`;
    case "number":
    case "boolean":
    case "undefined":
      return String(value);
    case "symbol":
      return value.toString();
    default:
      // Objects (including null-prototype ones) and functions: never call their own toString.
      return value === null ? "null" : Object.prototype.toString.call(value);
  }
}

function checkModel(method: string, model: unknown): string {
  if (typeof model !== "string" || model.length === 0) {
    throw new UIError(`KeyedRecordStore.${method}: model must be a non-empty string, got ${describeKey(model)}`, "usage");
  }
  return model;
}

/**
 * One model's records in a {@link KeyedRecordStore}: a row per record id, each holding that record's field
 * values. Signals are allocated only for coordinates read inside a reactive scope (as `ColumnarRecordStore`),
 * and a delete touches only the deleted record's row (6).
 *
 * Ids are never coerced: `1` and `"1"` are different records. A malformed id or field name is a `UIError`
 * (`usage`). Only `get` inside a reactive scope is tracked; `has`, `hasRecord`, `recordIds` and `size` are
 * untracked reads, so a list derived from them does not update by itself.
 */
export class ModelTable {
  readonly model: string;
  private readonly rows = new Map<RecordId, KeyedRow>();
  private liveRecords = 0;
  private retainedSignals = 0;

  /** @internal Tables are created by {@link KeyedRecordStore.table}; the package exports only the type. */
  constructor(model: string, token: typeof TABLE_TOKEN) {
    if (token !== TABLE_TOKEN) throw new UIError("ModelTable: create tables with KeyedRecordStore.table(model)", "usage");
    this.model = model;
  }

  /** Records holding at least one value. */
  get size(): number {
    return this.liveRecords;
  }

  /** Retained reactive coordinates of this table. */
  get signalCount(): number {
    return this.retainedSignals;
  }

  /** @internal Rows held, including emptied rows kept alive by a subscribed reader (tests check row release). */
  get rowCount(): number {
    return this.rows.size;
  }

  /**
   * Reads a field; inside a reactive scope the read is tracked per (record, field). A tracked read of an unknown
   * record keeps an empty row with the coordinate's signal; a cached computed remains a legitimate subscriber
   * until it unlinks, and the row is released with its last signal.
   */
  get(id: RecordId, field: string): unknown {
    const key = this.checkId("get", id);
    this.checkField("get", field);
    const row = this.rows.get(key);
    if (getActiveSub() === undefined) return row === undefined ? undefined : row.values.get(field);
    const target = row ?? this.addRow(key);
    const signals = target.signals ?? (target.signals = new Map());
    let s = signals.get(field);
    if (s === undefined) {
      const created = watchedSignal(target.values.get(field), () => {
        if (target.signals?.get(field) !== created) return;
        target.signals.delete(field);
        this.retainedSignals--;
        this.dropIfEmpty(key, target);
      });
      s = created;
      signals.set(field, s);
      this.retainedSignals++;
    }
    return s();
  }

  /** Writes a field and notifies the coordinate's reader, if any. */
  set(id: RecordId, field: string, value: unknown): void {
    const key = this.checkId("set", id);
    this.checkField("set", field);
    const row = this.rows.get(key) ?? this.addRow(key);
    if (row.values.size === 0) this.liveRecords++;
    row.values.set(field, value);
    row.signals?.get(field)?.(value);
  }

  /** Whether the record holds a value for `field` (a stored `undefined` counts). */
  has(id: RecordId, field: string): boolean {
    const row = this.rows.get(this.checkId("has", id));
    this.checkField("has", field);
    return row !== undefined && row.values.has(field);
  }

  /** Whether the record holds any value. */
  hasRecord(id: RecordId): boolean {
    const row = this.rows.get(this.checkId("hasRecord", id));
    return row !== undefined && row.values.size > 0;
  }

  /** Ids of the records holding at least one value; the order is not specified. */
  *recordIds(): IterableIterator<RecordId> {
    for (const [id, row] of this.rows) {
      if (row.values.size > 0) yield id;
    }
  }

  /**
   * Removes every value of one record, touching only its row. A read coordinate keeps its signal, set to
   * `undefined`, while a reader is subscribed, so a later `set` reaches it (review round 3, R3); the row is
   * dropped when its last signal is released. Deleting an unknown record does nothing.
   */
  deleteRecord(id: RecordId): void {
    const key = this.checkId("deleteRecord", id);
    const row = this.rows.get(key);
    if (row === undefined) return;
    if (row.values.size > 0) this.liveRecords--;
    row.values.clear();
    const signals = row.signals;
    if (signals !== undefined && signals.size > 0) {
      batch(() => {
        for (const s of signals.values()) s(undefined);
      });
    }
    this.dropIfEmpty(key, row);
  }

  /** Removes `field` from every record of this table; read coordinates keep their signals (see `deleteRecord`). */
  deleteField(field: string): void {
    this.checkField("deleteField", field);
    batch(() => {
      for (const [key, row] of this.rows) {
        if (row.values.delete(field) && row.values.size === 0) this.liveRecords--;
        row.signals?.get(field)?.(undefined);
        this.dropIfEmpty(key, row);
      }
    });
  }

  private addRow(key: RecordId): KeyedRow {
    const row: KeyedRow = { values: new Map(), signals: undefined };
    this.rows.set(key, row);
    return row;
  }

  private dropIfEmpty(key: RecordId, row: KeyedRow): void {
    if (row.values.size > 0 || (row.signals !== undefined && row.signals.size > 0)) return;
    if (this.rows.get(key) === row) this.rows.delete(key);
  }

  private checkField(method: string, field: unknown): void {
    if (typeof field !== "string" || field.length === 0) {
      throw new UIError(
        `ModelTable(${this.model}).${method}: field must be a non-empty string, got ${describeKey(field)}`,
        "usage",
      );
    }
  }

  private checkId(method: string, id: unknown): RecordId {
    if ((typeof id === "number" && Number.isSafeInteger(id)) || (typeof id === "string" && id.length > 0)) {
      return id;
    }
    throw new UIError(
      `ModelTable(${this.model}).${method}: record id must be a safe integer or a non-empty string, got ${describeKey(id)}`,
      "usage",
    );
  }
}

/**
 * Reactive record tables keyed by model, then record id (6). Two models with equal ids never share
 * a coordinate, and deleting a record touches only that record's row in its own table.
 */
export class KeyedRecordStore {
  private readonly tables = new Map<string, ModelTable>();

  /** The model's table, created on first use. */
  table(model: string): ModelTable {
    const name = checkModel("table", model);
    let table = this.tables.get(name);
    if (table === undefined) {
      table = new ModelTable(name, TABLE_TOKEN);
      this.tables.set(name, table);
    }
    return table;
  }

  /** The model's table if one was created, else `undefined`. */
  peekTable(model: string): ModelTable | undefined {
    return this.tables.get(checkModel("peekTable", model));
  }

  /** Models that have a table, in creation order. */
  models(): IterableIterator<string> {
    return this.tables.keys();
  }
}

/** A keyed selector: `isSelected(key)` reads (and tracks) whether `key` is selected; `dispose()` stops it. */
export interface Selector<T> {
  (key: T): boolean;
  /** Stops following the source and drops every key's signal; a later read is a `UIError` (`usage`). */
  readonly dispose: () => void;
}

/**
 * Creates an inverted selection index (§3.3): one boolean signal per key read, so a change of `source`
 * notifies only the readers of keys whose answer changed. With the default identity comparison a
 * change writes two signals (the old key's and the new one's), O(1) in the number of keys. A custom
 * `equals(key, value)` cannot be looked up by identity, so a change re-evaluates it for every still-subscribed key
 * (O(keys) comparisons, still only the changed readers re-run, review round 3, R14).
 *
 * Inside a component the selector stops with it; outside one, call `dispose()`. The signal of a key is
 * reclaimed when its last reactive dependency unlinks. Untracked direct reads retain no key signal;
 * cached computed dependencies remain valid readers. `dispose()` releases every remaining key.
 */
export function createSelector<T>(source: () => T, equals?: (key: T, value: T) => boolean): Selector<T> {
  const signals = new Map<T, CoreSignal<boolean>>();
  let current = untrack(source);
  let disposed = false;
  const matches = (key: T, value: T): boolean => (equals === undefined ? key === value : equals(key, value));

  const stop = effect(() => {
    const next = source();
    const prev = current;
    current = next;
    untrack(() =>
      batch(() => {
        if (equals === undefined) {
          if (prev === next) return;
          signals.get(prev)?.(false);
          signals.get(next)?.(true);
        } else {
          for (const [key, s] of signals) s(equals(key, next));
        }
      }),
    );
  });

  const selector = (key: T): boolean => {
    if (disposed) throw new UIError("createSelector: this selector was disposed; create a new one", "usage");
    if (getActiveSub() === undefined) return matches(key, current);
    let s = signals.get(key);
    if (s === undefined) {
      const created = watchedSignal(matches(key, current), () => {
        if (signals.get(key) === created) signals.delete(key);
      });
      s = created;
      signals.set(key, s);
    }
    return s();
  };
  const dispose = (): void => {
    if (disposed) return;
    disposed = true;
    stop();
    signals.clear();
  };
  // Inside a component the selector is disposed with it, so a later read fails loudly instead of
  // returning the values frozen at destruction (delta review N3).
  getOwner()?.onCleanup(dispose);
  return Object.assign(selector, { dispose }) as Selector<T>;
}
