/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { describe, expect, it } from "vitest";
import {
  Budget,
  computeDeepBytes,
  computeDepth,
  computeNodes,
  dictVal,
  floatVal,
  intVal,
  listVal,
  NONE,
  SLOT,
  strVal,
  TRUE,
  tupleVal,
  bytesVal,
} from "../src/index";

describe(" portable logical byte accounting", () => {
  it("computes exact logical deep bytes for scalars", () => {
    expect(computeDeepBytes(NONE)).toBe(32);
    expect(computeDeepBytes(TRUE)).toBe(32);
    expect(computeDeepBytes(intVal(0n))).toBe(32);
    expect(computeDeepBytes(intVal(9223372036854775807n))).toBe(32);
    expect(computeDeepBytes(floatVal(3.14))).toBe(32);
  });

  it("computes UTF-8 length for strings and byte length for bytes", () => {
    expect(computeDeepBytes(strVal(""))).toBe(32);
    expect(computeDeepBytes(strVal("hello"))).toBe(37);
    // 'héllo' has 5 chars, but 'é' is 2 UTF-8 bytes -> total 6 bytes -> 32 + 6 = 38
    expect(computeDeepBytes(strVal("héllo"))).toBe(38);

    expect(computeDeepBytes(bytesVal([]))).toBe(32);
    expect(computeDeepBytes(bytesVal([1, 2, 3]))).toBe(35);
  });

  it("computes container deep bytes matching Rust erp-expr Meta", () => {
    // List: 32 + sum(child.deep_size)
    const emptyList = listVal([]);
    expect(computeDeepBytes(emptyList)).toBe(32);

    const intList = listVal([intVal(1n), intVal(2n), intVal(3n)]);
    expect(computeDeepBytes(intList)).toBe(128); // 32 + 3 * 32

    const mixedTuple = tupleVal([intVal(1n), strVal("abc")]);
    expect(computeDeepBytes(mixedTuple)).toBe(99); // 32 + 32 + 35

    // Dict: 32 + sum(key.deep_size + val.deep_size + 16)
    const dict = dictVal([
      [strVal("a"), intVal(1n)], // key: 33, val: 32, overhead: 16 -> 81
      [strVal("b"), intVal(2n)], // key: 33, val: 32, overhead: 16 -> 81
    ]);
    expect(computeDeepBytes(dict)).toBe(194); // 32 + 81 + 81
  });

  it("enforces budget charging ledger and sticky quota limit failure", () => {
    const budget = new Budget({ maxTotalBytes: 200 });
    expect(budget.bytesUsed).toBe(0);
    expect(budget.stepsUsed).toBe(0);

    // Sequence pre-allocation reservation: 32 * (n + 1)
    budget.alloc(32 * 4); // 128
    expect(budget.bytesUsed).toBe(128);

    // Admit a string of 35 bytes (32 + 3)
    budget.admit(strVal("abc"));
    expect(budget.bytesUsed).toBe(163);
    expect(budget.stepsUsed).toBe(1);

    // Attempting to allocate 50 bytes exceeds 200 total budget
    expect(() => budget.alloc(50)).toThrow(/LimitExceeded: evaluation memory budget of 200 bytes exceeded/);

    // Once failed, budget is sticky: subsequent operations also fail
    expect(() => budget.tick()).toThrow(/LimitExceeded/);
  });
});
