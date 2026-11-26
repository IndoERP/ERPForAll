/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// @vitest-environment node
// Pin the complete runtime and compiler ABI. Review a change to these pins using the module
// attribution from scripts/size.mjs --inputs. See the runtime bundle budget in docs/development.md.

import { expect, test } from "vitest";
import { measure } from "../scripts/size.mjs";

const GZIP_PIN = 32_025;
/** Minified bytes of the bundle. */
const MINIFIED_PIN = 98_556;
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
  expect(minified, `measured ${minified} minified bytes against the ${MINIFIED_PIN}-byte pin`).toBe(
    MINIFIED_PIN,
  );
});
