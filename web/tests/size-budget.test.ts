/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// @vitest-environment node
// Pinned size of the runtime bundle. Two checks (review L9): the bundle
// is within its pin plus a small tolerance for zlib differences between hosts (esbuild and Node are
// pinned, so the minified bytes are exact), and a ratchet that fails once the bundle is clearly below
// the pin, so a change that shrinks it lowers the pin in the same commit (and).
// Raising a pin is a reviewed decision recorded in the slice's review file. The ≤ 8 KB gzip target is
// 's assertion. Legal comments must survive minification.

import { expect, test } from "vitest";
import { measure } from "../scripts/size.mjs";

const GZIP_PIN = 13_379;
/** Minified bytes of the bundle. */
const MINIFIED_PIN = 37_203;
/** Allowed difference of the gzip size between hosts (zlib builds); the minified size has none. */
const GZIP_TOLERANCE = 32;

const result = measure();

test("the runtime bundle is within its pinned size", async () => {
  const { minified, gzip } = await result;
  console.log(`runtime bundle: ${minified} bytes minified, ${gzip} bytes gzip -9`);
  expect(gzip).toBeLessThanOrEqual(GZIP_PIN + GZIP_TOLERANCE);
  expect(minified).toBeLessThanOrEqual(MINIFIED_PIN);
});

test("the size pins are lowered when the bundle shrinks (ratchet)", async () => {
  const { minified, gzip } = await result;
  expect(gzip, `the bundle shrank to ${gzip} bytes gzip: lower GZIP_PIN to ${gzip}`).toBeGreaterThanOrEqual(
    GZIP_PIN - GZIP_TOLERANCE,
  );
  expect(minified, `the bundle shrank to ${minified} bytes minified: lower MINIFIED_PIN to ${minified}`).toBe(
    MINIFIED_PIN,
  );
});
