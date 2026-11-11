/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { Budget, SLOT, computeDeepBytes, computeDepth, utf8ByteLength } from "./budget";
import { typeError } from "./python-error";
import { type ExprValue, intVal, strVal, pythonTypeOf } from "./values";

/** Yield one charged item at a time, without an unbounded intermediate character array. */
export function* iterateItems(value: ExprValue, budget: Budget): Generator<ExprValue> {
  if (value.kind === "str") {
    for (const ch of value.value) {
      budget.alloc(SLOT + utf8ByteLength(ch));
      budget.tick();
      yield strVal(ch);
    }
  } else if (value.kind === "bytes") {
    for (let index = 0; index < value.value.length; index++) {
      budget.alloc(SLOT);
      budget.tick();
      yield intVal(value.value[index]!);
    }
  } else if (value.kind === "list" || value.kind === "tuple") {
    for (const item of value.items) yield budget.admit(item);
  } else if (value.kind === "dict") {
    for (const [key] of value.entries) yield budget.admit(key);
  } else {
    throw typeError(`'${pythonTypeOf(value)}' object is not iterable`);
  }
}

/** Rust methods::iterable_items events: each item is admitted before copying; caller reserves its wrapper. */
export function collectItems(value: ExprValue, budget: Budget): ExprValue[] {
  let bytes = SLOT;
  let depth = 0;
  function plan(itemBytes: number, itemDepth: number): void {
    bytes += itemBytes;
    depth = Math.max(depth, itemDepth + 1);
    budget.checkSize(bytes);
    budget.checkDepth(depth);
  }
  if (value.kind === "str") {
    for (const ch of value.value) plan(SLOT + utf8ByteLength(ch), 0);
  } else if (value.kind === "bytes") {
    const count = computeDeepBytes(value) - SLOT;
    budget.checkSize(SLOT * (count + 1));
    if (count > 0) budget.checkDepth(1);
  } else if (value.kind === "list" || value.kind === "tuple") {
    for (const item of value.items) plan(computeDeepBytes(item), computeDepth(item));
  } else if (value.kind === "dict") {
    for (const [key] of value.entries) plan(computeDeepBytes(key), computeDepth(key));
  } else {
    throw typeError(`'${pythonTypeOf(value)}' object is not iterable`);
  }
  const output: ExprValue[] = [];
  if (value.kind === "str") {
    for (const ch of value.value) {
      budget.alloc(SLOT + utf8ByteLength(ch));
      budget.tick();
      output.push(strVal(ch));
    }
  } else if (value.kind === "bytes") {
    const count = computeDeepBytes(value) - SLOT;
    for (let index = 0; index < count; index++) {
      budget.alloc(SLOT); budget.tick();
      output.push(intVal(value.value[index]!));
    }
  } else if (value.kind === "list" || value.kind === "tuple") {
    for (const item of value.items) output.push(budget.admit(item));
  } else if (value.kind === "dict") {
    for (const [key] of value.entries) output.push(budget.admit(key));
  }
  return output;
}
