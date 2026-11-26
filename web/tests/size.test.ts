/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// @vitest-environment node
// Legal notices must survive the shipped minification path on every CI run.
import { expect, test } from "vitest";
import { analyzeEntry, bundle } from "../scripts/size.mjs";

test("the minified runtime bundle keeps every legal notice", async () => {
  const code = Buffer.from(await bundle()).toString("utf8");
  expect(code).toContain("alien-signals 3.2.1");
  expect(code).toContain("Copyright (c) 2024-present Johnson Chu");
  expect(code).toContain("dom-expressions 0.40.12");
  expect(code).toContain("Copyright (c) 2018-2019 Ryan Carniato");
  expect(code).toContain("Andrea Giammarchi");
  expect(code).toContain("SPDX-License-Identifier: MIT OR Apache-2.0");
});

test("importing a signal omits the optional restricted card renderer and its allowlists", async () => {
  const { inputs } = await analyzeEntry('export { signal } from "./src/index.ts";');
  const cardBytes = inputs["src/runtime/restricted-template.ts"]?.bytesInOutput ?? 0;
  // The pinned bundler preserves the module's inline license even when it removes every instruction.
  const licenseBytes = Buffer.byteLength("/*! SPDX-License-Identifier: MIT OR Apache-2.0 */");
  expect(cardBytes).toBeLessThanOrEqual(licenseBytes);
});
