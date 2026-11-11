/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The npm licence policy.

import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { expect, test } from "vitest";
import { check, satisfies } from "../scripts/check-licenses.mjs";

const root = join(dirname(fileURLToPath(import.meta.url)), "../..");
const lists = JSON.parse(readFileSync(join(root, "web/licenses.json"), "utf8")) as { production: string[]; dev_only: string[] };

test("the production list is exactly deny.toml's allow list", () => {
  const deny = readFileSync(join(root, "deny.toml"), "utf8");
  const block = /\[licenses\][\s\S]*?\nallow = \[([\s\S]*?)\]/.exec(deny);
  expect(block).not.toBeNull();
  const allow = [...block![1]!.matchAll(/"([^"]+)"/g)].map((m) => m[1]);
  expect([...lists.production].sort()).toEqual([...allow].sort());
  for (const l of lists.dev_only) expect(allow).not.toContain(l);
});

test("SPDX OR needs one allowed alternative, AND every part, WITH matches as a whole", () => {
  const allowed = new Set(["MIT", "Apache-2.0", "Apache-2.0 WITH LLVM-exception"]);
  expect(satisfies("MIT", allowed)).toBe(true);
  expect(satisfies("(MIT OR GPL-3.0-only)", allowed)).toBe(true);
  expect(satisfies("MIT AND GPL-3.0-only", allowed)).toBe(false);
  expect(satisfies("(MIT AND Apache-2.0) OR BSD-3-Clause", allowed)).toBe(true);
  expect(satisfies("Apache-2.0 WITH LLVM-exception", allowed)).toBe(true);
  expect(satisfies("GPL-2.0-only WITH Classpath-exception-2.0", allowed)).toBe(false);
  expect(satisfies("Unknown", allowed)).toBe(false);
  expect(() => satisfies("(MIT", allowed)).toThrow(/unbalanced/);
  expect(() => satisfies("MIT OR", allowed)).toThrow(/cannot parse/);
  expect(() => satisfies("", allowed)).toThrow(/empty/);
});

test("a production package needs a production licence; a dev package may use a dev-only one", () => {
  const pkg = (licence: string, name = "p") => ({ licence, name, versions: ["1.0.0"] });
  const base = { production: lists.production, devOnly: lists.dev_only, vendored: [] };
  expect(check({ ...base, prodPackages: [], allPackages: [pkg("CC0-1.0", "data")] })).toEqual([]);
  expect(check({ ...base, prodPackages: [pkg("CC0-1.0", "data")], allPackages: [pkg("CC0-1.0", "data")] })).toEqual([
    'data@1.0.0: licence "CC0-1.0" is not allowed for a production dependency',
  ]);
  expect(check({ ...base, prodPackages: [], allPackages: [pkg("GPL-3.0-only", "gpl")] })).toEqual([
    'gpl@1.0.0: licence "GPL-3.0-only" is not allowed for a dev dependency',
  ]);
  expect(check({ ...base, prodPackages: [], allPackages: [], vendored: [pkg("MIT AND ISC", "vendor/x")] })).toEqual([]);
});

test("a package whose licence pnpm reports as Unknown, or that has none, fails the check (review L7)", () => {
  const pkg = (licence: string, name: string) => ({ licence, name, versions: ["1.0.0"] });
  const base = { production: lists.production, devOnly: lists.dev_only, vendored: [] };
  expect(check({ ...base, prodPackages: [], allPackages: [pkg("Unknown", "nolicence")] })).toEqual([
    'nolicence@1.0.0: licence "Unknown" is not allowed for a dev dependency',
  ]);
  expect(check({ ...base, prodPackages: [pkg("UNKNOWN", "upper")], allPackages: [pkg("UNKNOWN", "upper")] })).toEqual([
    'upper@1.0.0: licence "UNKNOWN" is not allowed for a production dependency',
  ]);
  expect(check({ ...base, prodPackages: [], allPackages: [pkg("", "empty")] })).toEqual([
    "empty@1.0.0: empty licence expression",
  ]);
});
