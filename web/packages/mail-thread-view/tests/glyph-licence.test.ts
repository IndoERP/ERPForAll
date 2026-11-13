/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The action glyph drawings are Lucide's: their licence notice must ship with the package that carries them.
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { expect, test } from "vitest";

const here = dirname(fileURLToPath(import.meta.url));

test("the package carries the Lucide licence notice next to the drawings it copies", () => {
  const notice = readFileSync(join(here, "../LICENSE-LUCIDE"), "utf8");
  expect(notice).toContain("ISC License");
  expect(notice).toContain("Lucide Icons and Contributors");
  expect(readFileSync(join(here, "../src/dom.ts"), "utf8")).toContain("LICENSE-LUCIDE");
});
