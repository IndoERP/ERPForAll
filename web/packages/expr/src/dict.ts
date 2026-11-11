/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { type Budget, computeDeepBytes, computeNodes } from "./budget";
import { keyError, typeError } from "./python-error";
import {
  type ExprValue,
  dictVal,
  repr,
  tupleVal,
} from "./values";

export function isHashable(val: ExprValue): boolean {
  switch (val.kind) {
    case "list":
    case "dict":
      return false;
    case "tuple":
      for (const item of val.items) {
        if (!isHashable(item)) return false;
      }
      return true;
    default:
      return true;
  }
}

export function assertHashable(val: ExprValue): void {
  if (!isHashable(val)) {
    throw typeError(`unhashable type: '${val.kind}'`);
  }
}

export function keysEqual(a: ExprValue, b: ExprValue): boolean {
  assertHashable(a);
  assertHashable(b);

  if (a.kind === "none" && b.kind === "none") return true;
  if (a.kind === "none" || b.kind === "none") return false;

  // Numeric equivalence across bool, int, float
  const aIsNum = a.kind === "bool" || a.kind === "int" || a.kind === "float";
  const bIsNum = b.kind === "bool" || b.kind === "int" || b.kind === "float";

  if (aIsNum && bIsNum) {
    // If either is NaN, in Python: nan == nan is False
    if (a.kind === "float" && Number.isNaN(a.value)) return false;
    if (b.kind === "float" && Number.isNaN(b.value)) return false;

    const aVal = a.kind === "bool" ? (a.value ? 1n : 0n) : a.kind === "int" ? a.value : null;
    const bVal = b.kind === "bool" ? (b.value ? 1n : 0n) : b.kind === "int" ? b.value : null;

    if (aVal !== null && bVal !== null) {
      return aVal === bVal;
    }

    const aFloat = a.kind === "float" ? a.value : a.kind === "bool" ? (a.value ? 1 : 0) : Number(a.value);
    const bFloat = b.kind === "float" ? b.value : b.kind === "bool" ? (b.value ? 1 : 0) : Number(b.value);

    // If int exceeds safe integer representation, check exact equality
    if (a.kind === "int" && b.kind === "float") {
      if (!Number.isFinite(b.value)) return false;
      return BigInt(Math.trunc(b.value)) === a.value && Number(a.value) === b.value;
    }
    if (a.kind === "float" && b.kind === "int") {
      if (!Number.isFinite(a.value)) return false;
      return BigInt(Math.trunc(a.value)) === b.value && Number(b.value) === a.value;
    }

    return aFloat === bFloat;
  }

  if (a.kind === "str" && b.kind === "str") {
    return a.value === b.value;
  }

  if (a.kind === "bytes" && b.kind === "bytes") {
    if (a.value.length !== b.value.length) return false;
    for (let i = 0; i < a.value.length; i++) {
      if (a.value[i] !== b.value[i]) return false;
    }
    return true;
  }

  if (a.kind === "tuple" && b.kind === "tuple") {
    if (a.items.length !== b.items.length) return false;
    for (let i = 0; i < a.items.length; i++) {
      if (!keysEqual(a.items[i]!, b.items[i]!)) return false;
    }
    return true;
  }

  return false;
}

/** Primitive numeric keys share Python equality; compound keys use a charged comparison walk. */
function primitiveToken(value: ExprValue): string | undefined {
  if (value.kind === "str") return `s:${value.value}`;
  if (value.kind === "none") return "none";
  if (value.kind === "int") return `n:${value.value}`;
  if (value.kind === "bool") return `n:${value.value ? 1 : 0}`;
  if (value.kind === "float" && !Number.isNaN(value.value)) {
    return Number.isFinite(value.value) && Number.isInteger(value.value) ? `n:${BigInt(value.value)}` : `f:${value.value}`;
  }
  return undefined;
}

export class PyDict {
  readonly #entries: [ExprValue, ExprValue][] = [];
  readonly #primitive = new Map<string, number>();
  readonly #compound: number[] = [];

  constructor(entries: readonly (readonly [ExprValue, ExprValue])[] = [], budget?: Budget) {
    for (const [key, value] of entries) this.set(key, value, budget);
  }
  get length(): number { return this.#entries.length; }
  get entries(): readonly (readonly [ExprValue, ExprValue])[] { return this.#entries; }

  private index(key: ExprValue, budget?: Budget): number | undefined {
    if (budget !== undefined) budget.checkValue(key);
    assertHashable(key);
    const token = primitiveToken(key);
    if (token !== undefined) { budget?.tick(); return this.#primitive.get(token); }
    if (key.kind === "float") return undefined; // NaN never compares equal in the existing key contract.
    for (const index of this.#compound) {
      const previous = this.#entries[index]![0];
      if (budget !== undefined) {
        budget.steps(computeNodes(previous) + computeNodes(key));
        budget.scan(computeDeepBytes(previous) + computeDeepBytes(key));
      }
      if (keysEqual(previous, key)) return index;
    }
    return undefined;
  }
  has(key: ExprValue, budget?: Budget): boolean { return this.index(key, budget) !== undefined; }
  get(key: ExprValue, defaultValue?: ExprValue, budget?: Budget): ExprValue | undefined {
    const index = this.index(key, budget);
    return index === undefined ? defaultValue : this.#entries[index]![1];
  }
  getItem(key: ExprValue, budget?: Budget): ExprValue {
    const value = this.get(key, undefined, budget);
    if (value === undefined) throw keyError(repr(key));
    return value;
  }
  set(key: ExprValue, value: ExprValue, budget?: Budget): void {
    const index = this.index(key, budget);
    if (index !== undefined) {
      this.#entries[index] = [this.#entries[index]![0], value];
      return;
    }
    const next = this.#entries.length;
    const token = primitiveToken(key);
    if (token !== undefined) this.#primitive.set(token, next);
    else if (key.kind !== "float") this.#compound.push(next);
    this.#entries.push([key, value]);
  }

  keys(): ExprValue[] {
    return this.#entries.map(([k]) => k);
  }

  values(): ExprValue[] {
    return this.#entries.map(([, v]) => v);
  }

  items(): ExprValue[] {
    return this.#entries.map(([k, v]) => tupleVal([k, v]));
  }

  merge(other: PyDict): PyDict {
    const res = new PyDict(this.#entries);
    for (const [k, v] of other.entries) {
      res.set(k, v);
    }
    return res;
  }

  toExprValue(): ExprValue {
    return dictVal(this.#entries);
  }
}
