/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The frozen helper ABI: the contract declares exactly the 17
// helpers of the compiler note §7.2 and the implementation module exports exactly those names
// (tsc checks the signatures through src/runtime/abi-conformance.ts).

import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { expect, test } from "vitest";
import * as abi from "@erpforall/elang/abi";

const HELPERS = [
  "template",
  "effect",
  "insert",
  "out",
  "attr",
  "cls",
  "style",
  "prop",
  "attrs",
  "interp",
  "on",
  "callHandler",
  "cond",
  "list",
  "keyed",
  "component",
  "slot",
];

test("the ABI module exports exactly the 17 helpers, each a function", () => {
  expect(HELPERS).toHaveLength(17);
  expect(Object.keys(abi).sort()).toEqual([...HELPERS].sort());
  for (const name of HELPERS) expect(typeof (abi as Record<string, unknown>)[name], name).toBe("function");
});

test("abi.d.ts declares exactly the 17 helpers", () => {
  const src = readFileSync(join(dirname(fileURLToPath(import.meta.url)), "../src/runtime/abi.d.ts"), "utf8");
  const declared = [...src.matchAll(/^export declare function (\w+)\(/gm)].map((m) => m[1]);
  expect(declared).toEqual(HELPERS);
  expect(src).not.toMatch(/^export (?!declare function|interface)/m);
});
