/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import assert from "node:assert/strict";
import { test } from "vitest";
import { contrastOnLayers } from "../scripts/browser-office-measurements.mjs";

test("contrast composites translucent foreground and nested surfaces", () => {
  const actual = contrastOnLayers("rgba(0, 0, 0, 0.5)", ["rgba(0, 0, 255, 0.2)", "rgb(255, 255, 255)"]);
  assert.deepEqual(actual.background, [204, 204, 255]);
  assert.deepEqual(actual.foreground, [102, 102, 127.5]);
  assert.ok(actual.ratio > 3.5 && actual.ratio < 4);
});

test("transparent ancestor layers retain the actual opaque backing", () => {
  const actual = contrastOnLayers("rgb(255, 255, 255)", ["rgba(0, 0, 0, 0)", "rgb(0, 0, 0)"]);
  assert.equal(actual.ratio, 21);
});

test("missing backing or unsupported colors fail instead of assuming white", () => {
  assert.throws(() => contrastOnLayers("rgb(0, 0, 0)", ["rgba(0, 0, 0, 0)"]),
    { name: "Error", message: "Contrast needs an opaque ancestor background." });
  assert.throws(() => contrastOnLayers("currentColor", ["rgb(255, 255, 255)"]),
    { name: "Error", message: "Unsupported computed color: currentColor" });
});

test("computed custom properties accept sRGB hex colors including alpha", () => {
  assert.equal(contrastOnLayers("#ffffff", ["#000000"]).ratio, 21);
  assert.deepEqual(contrastOnLayers("#00000080", ["#ffffff"]).foreground, [127, 127, 127]);
});
